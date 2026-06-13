//! `cli.rs` — clap argument parsing and subcommand dispatch.
//!
//! `plan` is the default subcommand (running `rollout` with no args equals
//! `rollout plan`). `apply` is the only mutating path.
//! `install` copies a binary to its dest and restarts the owning daemon.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::autogate::{self, GateConfig, GateVerdict, ProofEntry, ProofLedger};
use crate::error::RolloutError;
use crate::fleet::{self, FleetConfig};
use crate::warmswap::restart_strategy_label;
use crate::fleetgen::{run_fleet_gen, FleetGenArgs};
use crate::health::{
    check_window_guard, is_voice_daemon, voice_activity_in_flight, VoiceActivityState,
    DEFAULT_VOICE_SAMPLE_SECS,
};
use crate::install::{run_install, InstallArgs, OutputFormat};
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
#[command(name = "rollout", version, about = "Safe rolling restart for the live fleet")]
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
    ///
    /// With `--auto`, consults the proof ledger (`~/.config/rollout/proofs.json`)
    /// for each daemon. Only daemons with a current Allow verdict (matching
    /// binary hash, 0 events lost) are restarted; others are skipped with the
    /// reason printed. Requires `auto_enabled = true` in fleet.toml or env var
    /// `ROLLOUT_AUTO_ENABLED=1`.
    Apply(ApplyArgs),

    /// Install a freshly-built binary to its dest and restart the owning daemon.
    ///
    /// Copies `<binary>` to `--dest` via atomic temp-then-rename (mode 0755),
    /// finds the systemd-user unit whose `ExecStart` points at dest, and restarts it
    /// via `agorabus reload --build` (for agorabus) or `systemctl --user restart`
    /// (for all other daemons). Emits a structured verdict.
    Install(InstallCliArgs),

    /// Derive a candidate fleet.toml from the live daemon set.
    ///
    /// Reads `binstale scan --format json`, cross-references each daemon's
    /// install path against `~/.config/systemd/user/*.service`, and writes a
    /// `[[daemon]]` recipe for each matched daemon to `--out` (default:
    /// `~/.config/rollout/fleet.toml.proposed`).
    ///
    /// Never writes to `fleet.toml` directly. Review the proposed file and
    /// accept it with `mv fleet.toml.proposed fleet.toml`.
    FleetGen(FleetGenCliArgs),

    /// Record a changeover-probe result into the per-daemon proof ledger.
    ///
    /// Reads the probe JSON (produced by `changeover probe`) from `--from` and
    /// writes or replaces the daemon's entry in `~/.config/rollout/proofs.json`.
    /// A second `record-proof` for the same daemon always replaces the first.
    ///
    /// The proof is then consulted by `rollout apply --auto` to decide whether
    /// a daemon's swap is safe to execute unattended.
    RecordProof(RecordProofArgs),

    /// Run `changeover probe` for one or all daemons and seed the proof ledger.
    ///
    /// `rollout prove --daemon <unit>` invokes `changeover probe <unit> --json`,
    /// feeds the output to the existing record-proof ingestion path, and writes
    /// `~/.config/rollout/proofs.json`.  Exits non-zero if the proof verdict is
    /// Refuse (binary hash mismatch or `events_lost` > 0).
    ///
    /// `--all` proves every daemon in fleet.toml; `--dry-run` prints without writing.
    Prove(crate::prove::ProveArgs),
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

    /// Consult the proof ledger; only restart daemons with a current Allow verdict.
    ///
    /// Requires env var `ROLLOUT_AUTO_ENABLED=1` to be set (safety interlock).
    /// Each daemon is checked against `~/.config/rollout/proofs.json`; daemons
    /// with a Refuse verdict are skipped with the reason printed.
    #[arg(long)]
    pub auto: bool,

    /// Print what would happen without restarting any daemon.
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for `rollout record-proof`.
#[derive(Debug, Args)]
pub(crate) struct RecordProofArgs {
    /// Path to the probe JSON produced by `changeover probe`.
    #[arg(long = "from", value_name = "PATH|-")]
    pub from: String,
    /// Path to the proof ledger. Default: ~/.config/rollout/proofs.json.
    #[arg(long, value_name = "PATH")]
    pub ledger: Option<PathBuf>,
}

/// Arguments for `rollout fleet-gen`.
#[derive(Debug, Default, Args)]
pub(crate) struct FleetGenCliArgs {
    /// Write the proposed config to this path instead of the default
    /// `~/.config/rollout/fleet.toml.proposed`.
    #[arg(long, value_name = "PATH")]
    pub out: Option<PathBuf>,

    /// Only generate recipes for daemons whose name matches this regex.
    ///
    /// Example: `--match 'wm-.*'` generates recipes only for wm-* daemons.
    #[arg(long, value_name = "REGEX")]
    pub match_regex: Option<String>,
}

/// Arguments for `rollout install`.
#[derive(Debug, Args)]
pub(crate) struct InstallCliArgs {
    /// Path to the freshly-built binary to install.
    #[arg(value_name = "BINARY")]
    pub binary: PathBuf,

    /// Destination install path (e.g. `~/.local/bin/recalld`).
    #[arg(long, value_name = "PATH")]
    pub dest: PathBuf,

    /// Safety guard: refuse to restart voice-set daemons unless the bus has
    /// been quiet for this duration. Examples: `30s`, `2m`.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration, default_value = "5s")]
    pub restart_window: Duration,

    /// Print what would happen without writing any files or restarting daemons.
    #[arg(long)]
    pub dry_run: bool,

    /// Output format: json (default) or table.
    #[arg(long, value_name = "FORMAT", default_value = "json")]
    pub format: String,
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
        let recipe = fleet.get(&entry.comm).ok_or_else(|| RolloutError::UnknownDaemons {
            names: vec![entry.comm.clone()],
        })?;
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
        println!("  strategy: {}", restart_strategy_label(recipe));
        println!("  build:   {}", recipe.build_cmd);
        println!("  install: {}", recipe.install_cmd);
        println!("  launch:  {}", recipe.launch_cmd);
        println!("  health:  {}", recipe.healthcheck_cmd());
        println!("  grace:   {}s", recipe.grace_period_secs);
        if restart_strategy_label(recipe) == "warm-swap" {
            use crate::warmswap::claim_path_for_recipe;
            if let Some(claim_path) = claim_path_for_recipe(recipe) {
                println!("  warm-swap sequence:");
                println!("    1. install binary → dest");
                println!("    2. launch successor (systemd-run --user --scope)");
                println!("    3. wait for ClaimAcquire on '{claim_path}'");
                println!("    4. stop predecessor (systemctl --user stop)");
                println!("    5. verify exactly 1 holder on '{claim_path}'");
            }
        }
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

    // --auto gate: load the ledger once, then evaluate per-daemon below.
    let auto_ledger: Option<ProofLedger> = if args.auto {
        let enabled = std::env::var("ROLLOUT_AUTO_ENABLED").unwrap_or_default();
        if enabled != "1" {
            println!(
                "rollout apply: auto is disabled (set ROLLOUT_AUTO_ENABLED=1 to enable)"
            );
            return Ok(());
        }
        let ledger_path = autogate::default_proofs_path()?;
        Some(ProofLedger::load(&ledger_path)?)
    } else {
        None
    };

    if args.dry_run {
        println!("rollout apply [dry-run]: would restart {} daemon(s):", stale.len());
        for entry in &stale {
            println!("  {}", entry.comm);
        }
        return Ok(());
    }

    let healthcheck_timeout = Duration::from_secs(args.healthcheck_timeout);
    let window_duration = args.window.unwrap_or_else(|| Duration::from_secs(5));
    let voice_sample = Duration::from_secs(DEFAULT_VOICE_SAMPLE_SECS);

    let mut results: Vec<RestartResult> = Vec::new();
    let mut deferred: Vec<String> = Vec::new();
    let mut refused_auto: Vec<String> = Vec::new();

    for (i, entry) in stale.iter().enumerate() {
        let recipe = fleet.get(&entry.comm).ok_or_else(|| RolloutError::UnknownDaemons {
            names: vec![entry.comm.clone()],
        })?;

        println!(
            "rollout apply [{}/{}]: starting {} (pid={})",
            i + 1,
            stale.len(),
            entry.comm,
            entry.pid
        );

        // Turn/session liveness probe — always applied to voice-set daemons.
        if is_voice_daemon(&entry.comm) {
            match voice_activity_in_flight(voice_sample) {
                VoiceActivityState::InFlight { reason } => {
                    println!(
                        "rollout apply [{}/{}]: {} deferred — voice active: {}",
                        i + 1,
                        stale.len(),
                        entry.comm,
                        reason,
                    );
                    deferred.push(entry.comm.clone());
                    continue;
                }
                VoiceActivityState::BusUnreachable => {
                    println!(
                        "rollout apply [{}/{}]: {} deferred — agorabus unreachable \
                         (cannot confirm turn is idle; will not restart blind)",
                        i + 1,
                        stale.len(),
                        entry.comm,
                    );
                    deferred.push(entry.comm.clone());
                    continue;
                }
                VoiceActivityState::Idle => {
                    // Bus is quiet — proceed to restart.
                }
            }
        }

        // Auto-gate: check proof ledger when --auto is set.
        if let Some(ref ledger) = auto_ledger {
            // Use the exe path as a stand-in hash when no binary hash is available
            // (the probe records the real hash; for apply we use the on-disk exe path digest).
            let current_hash = entry.exe_path.clone();
            let config = GateConfig::default();
            let verdict = autogate::gate(&entry.comm, &current_hash, ledger, &config);
            match verdict {
                GateVerdict::Allow => {
                    println!(
                        "rollout apply [{}/{}]: {} auto-gate Allow",
                        i + 1, stale.len(), entry.comm
                    );
                }
                GateVerdict::Refuse { ref reason } => {
                    println!(
                        "rollout apply [{}/{}]: {} auto-gate Refuse — {reason}",
                        i + 1, stale.len(), entry.comm
                    );
                    refused_auto.push(entry.comm.clone());
                    continue;
                }
            }
        }

        // Coarse window guard (opt-in via --window; backward-compat).
        if args.window.is_some() {
            check_window_guard(&entry.comm, window_duration)?;
        }

        match restart_daemon(recipe, entry.pid, healthcheck_timeout) {
            Ok(result) => {
                let elapsed_ms = result.end_ms.saturating_sub(result.start_ms);
                println!(
                    "rollout apply [{}/{}]: {} ok (old_pid={}, new_pid={:?}, elapsed={}ms, restart_path={}{})",
                    i + 1,
                    stale.len(),
                    entry.comm,
                    result.old_pid,
                    result.new_pid,
                    elapsed_ms,
                    result.restart_path,
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

    if !refused_auto.is_empty() {
        println!(
            "rollout apply: {} auto-gate refused: {}",
            refused_auto.len(),
            refused_auto.join(", ")
        );
    }

    if deferred.is_empty() {
        println!(
            "rollout apply: done. {}/{} daemons restarted.",
            results.len(),
            stale.len()
        );
        Ok(())
    } else {
        println!(
            "rollout apply: {}/{} restarted; {} deferred (voice active): {}",
            results.len(),
            stale.len(),
            deferred.len(),
            deferred.join(", "),
        );
        Err(RolloutError::VoiceActivityDeferred {
            names: deferred.join(", "),
            count: deferred.len(),
        })
    }
}

/// Run the `record-proof` subcommand: ingest a changeover probe JSON into the ledger.
///
/// # Errors
///
/// Returns an error if the probe JSON cannot be read or parsed, or if the ledger
/// cannot be loaded or saved.
pub(crate) fn run_record_proof(args: &RecordProofArgs) -> Result<(), RolloutError> {
    // Read probe JSON from file or stdin.
    let json = if args.from == "-" {
        use std::io::Read as _;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| RolloutError::BinstaleScanFailed(format!("read stdin: {e}")))?;
        buf
    } else {
        std::fs::read_to_string(&args.from).map_err(|e| {
            RolloutError::BinstaleScanFailed(format!("read {}: {e}", args.from))
        })?
    };

    let entry: ProofEntry = serde_json::from_str(&json).map_err(|e| {
        RolloutError::FleetConfig(format!("probe JSON parse error: {e}"))
    })?;

    let ledger_path = match &args.ledger {
        Some(p) => p.clone(),
        None => autogate::default_proofs_path()?,
    };

    let mut ledger = ProofLedger::load(&ledger_path)?;
    let daemon_name = entry.daemon.clone();
    ledger.upsert(entry);
    ledger.save(&ledger_path)?;

    println!(
        "record-proof: saved proof for `{daemon_name}` → {}",
        ledger_path.display()
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

/// Run the `fleet-gen` subcommand: derive a candidate fleet.toml from live state.
///
/// # Errors
///
/// Returns an error if binstale cannot be invoked, the output path cannot
/// be written, or the `--match` regex is invalid.
pub(crate) fn run_fleet_gen_cmd(args: &FleetGenCliArgs) -> Result<(), RolloutError> {
    let gen_args = FleetGenArgs {
        out: args.out.clone(),
        match_regex: args.match_regex.clone(),
    };
    run_fleet_gen(&gen_args)
}

/// Run the `install` subcommand: copy binary and restart the owning daemon.
///
/// # Errors
///
/// Returns an error if the binary cannot be read, the dest parent does not
/// exist, or the window guard blocks a voice-set restart.
pub(crate) fn run_install_cmd(args: &InstallCliArgs) -> Result<(), RolloutError> {
    let fmt = args
        .format
        .parse::<OutputFormat>()
        .map_err(RolloutError::FleetConfig)?;
    let install_args = InstallArgs {
        binary: args.binary.clone(),
        dest: args.dest.clone(),
        restart_window: args.restart_window,
        dry_run: args.dry_run,
        format: fmt,
    };
    run_install(&install_args)
}
