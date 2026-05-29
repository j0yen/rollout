//! `cli.rs` — clap argument parsing and subcommand dispatch.
//!
//! `plan` is the default subcommand (running `rollout` with no args equals
//! `rollout plan`). `apply` is the only mutating path.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::error::RolloutError;
use crate::fleet::{self, FleetConfig};
use crate::health::check_window_guard;
use crate::restart::{restart_daemon, RestartResult, DEFAULT_HEALTHCHECK_TIMEOUT_SECS};
use crate::scan::{collect_stale, parse_stale_json, BinstaleEntry, ScanSource};

/// rollout — safe rolling restart for the live fleet.
///
/// Consumes `binstale scan --format json`, rebuilds and restarts stale daemons
/// one at a time, and confirms each one re-registers on agorabus before
/// proceeding to the next.
///
/// `plan` (the default) shows what would run without mutating anything.
/// `apply` executes the plan. A daemon with no recipe in fleet.toml is refused.
#[derive(Debug, Parser)]
#[command(name = "rollout", version = "0.1.0", about = "Safe rolling restart for the live fleet")]
pub(crate) struct Cli {
    /// Subcommand to run. Defaults to `plan` if omitted.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Rollout subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Print the ordered list of stale daemons and the exact commands that would run.
    ///
    /// This is the default subcommand — running `rollout` with no args is equivalent
    /// to `rollout plan`. No mutation ever occurs under `plan`.
    Plan(PlanArgs),

    /// Execute the restart plan, strictly serialized (one daemon at a time).
    ///
    /// Per daemon: build → install → SIGTERM → wait → relaunch → healthcheck.
    /// Stops on the first daemon that fails to re-register.
    Apply(ApplyArgs),
}

impl Default for Command {
    fn default() -> Self {
        Self::Plan(PlanArgs::default())
    }
}

/// Arguments for `rollout plan`.
#[derive(Debug, Default, Args)]
pub(crate) struct PlanArgs {
    /// Path to fleet.toml. Default: ~/.config/rollout/fleet.toml.
    #[arg(long, value_name = "PATH")]
    pub fleet: Option<PathBuf>,

    /// Read binstale JSON from a file path, or `-` for stdin.
    ///
    /// Example: `--from -` reads from stdin (pipeline-friendly).
    #[arg(long = "from", value_name = "PATH|-")]
    pub from: Option<String>,

    /// Only show this daemon (by name).
    #[arg(long, value_name = "NAME")]
    pub only: Option<String>,
}

/// Arguments for `rollout apply`.
#[derive(Debug, Args)]
pub(crate) struct ApplyArgs {
    /// Path to fleet.toml. Default: ~/.config/rollout/fleet.toml.
    #[arg(long, value_name = "PATH")]
    pub fleet: Option<PathBuf>,

    /// Read binstale JSON from a file path, or `-` for stdin.
    ///
    /// Example: `--from -` reads from stdin (pipeline-friendly).
    #[arg(long = "from", value_name = "PATH|-")]
    pub from: Option<String>,

    /// Only restart this daemon (by name).
    #[arg(long, value_name = "NAME")]
    pub only: Option<String>,

    /// Safety guard: refuse to restart voice-set daemons (wm-dialog|stt|tts) unless the
    /// agorabus bus has shown no `wm.dialog.turn.*` activity for this duration.
    ///
    /// Examples: `--window 30s`, `--window 2m`.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub window: Option<Duration>,

    /// Healthcheck timeout per daemon. Default: 30 seconds.
    #[arg(long, value_name = "SECS", default_value_t = DEFAULT_HEALTHCHECK_TIMEOUT_SECS)]
    pub healthcheck_timeout: u64,
}

/// Parse a human-readable duration like "30s", "2m", "1h".
fn parse_duration(s: &str) -> Result<Duration, String> {
    if let Some(n) = s.strip_suffix('s') {
        n.parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|e| format!("invalid seconds: {e}"))
    } else if let Some(n) = s.strip_suffix('m') {
        n.parse::<u64>()
            .map(|m| Duration::from_secs(m * 60))
            .map_err(|e| format!("invalid minutes: {e}"))
    } else if let Some(n) = s.strip_suffix('h') {
        n.parse::<u64>()
            .map(|h| Duration::from_secs(h * 3600))
            .map_err(|e| format!("invalid hours: {e}"))
    } else {
        Err(format!("unsupported duration format: {s:?}; use e.g. 30s, 2m, 1h"))
    }
}

/// Run the `plan` subcommand: print the restart plan without mutating.
///
/// # Errors
///
/// Returns an error if fleet.toml cannot be loaded or an unknown daemon is named.
pub(crate) fn run_plan(args: &PlanArgs) -> Result<(), RolloutError> {
    let fleet = load_fleet(args.fleet.as_deref())?;
    let stale = collect_stale_entries(args.from.as_deref())?;

    let stale = filter_only(stale, args.only.as_deref());
    if stale.is_empty() {
        println!("rollout plan: no stale daemons found. Nothing to do.");
        return Ok(());
    }

    // Validate all daemons have recipes before printing anything.
    fleet.validate_names(stale.iter().map(|e| e.comm.as_str()))?;

    println!("rollout plan: {} stale daemon(s) found", stale.len());
    println!();
    for (i, entry) in stale.iter().enumerate() {
        let recipe = fleet.get(&entry.comm).expect("validated above");
        println!(
            "[{}/{}] {} (pid={}, verdict={})",
            i + 1,
            stale.len(),
            entry.comm,
            entry.pid,
            entry.verdict
        );
        println!("  exe:     {}", entry.exe_path);
        if let Some(repo) = &recipe.repo {
            println!("  repo:    {}", repo.display());
        }
        println!("  build:   {}", recipe.build_cmd);
        println!("  install: {}", recipe.install_cmd);
        println!("  launch:  {}", recipe.launch_cmd);
        println!("  health:  {}", recipe.healthcheck_cmd());
        println!("  grace:   {}s", recipe.grace_period_secs);
        println!();
    }
    Ok(())
}

/// Run the `apply` subcommand: execute the restart plan.
///
/// # Errors
///
/// Returns an error on unknown daemons, build/install failure, signal errors,
/// or healthcheck timeout.
pub(crate) fn run_apply(args: &ApplyArgs) -> Result<(), RolloutError> {
    let fleet = load_fleet(args.fleet.as_deref())?;
    let stale = collect_stale_entries(args.from.as_deref())?;

    let stale = filter_only(stale, args.only.as_deref());
    if stale.is_empty() {
        println!("rollout apply: no stale daemons. Nothing to do.");
        return Ok(());
    }

    // Validate all daemons before touching any of them.
    fleet.validate_names(stale.iter().map(|e| e.comm.as_str()))?;

    let healthcheck_timeout = Duration::from_secs(args.healthcheck_timeout);
    let window_duration = args.window.unwrap_or_else(|| Duration::from_secs(5));

    let mut results: Vec<RestartResult> = Vec::new();

    for (i, entry) in stale.iter().enumerate() {
        let recipe = fleet.get(&entry.comm).expect("validated above");

        println!(
            "rollout apply [{}/{}]: starting {} (pid={})",
            i + 1,
            stale.len(),
            entry.comm,
            entry.pid
        );

        // Window guard for voice-set daemons.
        if args.window.is_some() {
            check_window_guard(&entry.comm, window_duration)?;
        }

        match restart_daemon(recipe, entry.pid, healthcheck_timeout) {
            Ok(result) => {
                let elapsed_ms = result.end_ms.saturating_sub(result.start_ms);
                println!(
                    "rollout apply [{}/{}]: {} ok (old_pid={}, new_pid={:?}, elapsed={}ms{})",
                    i + 1,
                    stale.len(),
                    entry.comm,
                    result.old_pid,
                    result.new_pid,
                    elapsed_ms,
                    if result.sigkill_used { ", SIGKILL used" } else { "" }
                );
                results.push(result);
            }
            Err(e) => {
                eprintln!(
                    "rollout apply [{}/{}]: {} FAILED — {e}",
                    i + 1,
                    stale.len(),
                    entry.comm
                );
                eprintln!("rollout apply: stopping run after first failure");
                return Err(e);
            }
        }
    }

    println!(
        "rollout apply: done. {}/{} daemons restarted.",
        results.len(),
        stale.len()
    );
    Ok(())
}

/// Load fleet config from the given path or the default path.
fn load_fleet(path: Option<&std::path::Path>) -> Result<FleetConfig, RolloutError> {
    let p = match path {
        Some(p) => p.to_owned(),
        None => fleet::default_fleet_path()?,
    };
    FleetConfig::load(&p)
}

/// Collect stale entries from a source path, stdin, or via binstale.
///
/// If `from` is `Some("-")`, reads from stdin.
/// If `from` is `Some(path)`, reads from the given file path.
/// If `from` is `None`, shells out to `binstale scan --format json`.
fn collect_stale_entries(from: Option<&str>) -> Result<Vec<BinstaleEntry>, RolloutError> {
    match from {
        Some("-") => collect_stale(&ScanSource::Stdin),
        Some(path) => {
            let json = std::fs::read_to_string(path)
                .map_err(|e| RolloutError::BinstaleScanFailed(format!("read {path}: {e}")))?;
            parse_stale_json(&json)
        }
        None => collect_stale(&ScanSource::Binstale { match_regex: None }),
    }
}

/// Filter entries to only the named daemon if `--only` was set.
fn filter_only(entries: Vec<BinstaleEntry>, only: Option<&str>) -> Vec<BinstaleEntry> {
    match only {
        Some(name) => entries.into_iter().filter(|e| e.comm == name).collect(),
        None => entries,
    }
}
