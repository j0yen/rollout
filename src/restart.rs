//! `restart.rs` — the serialized build→install→SIGTERM→relaunch→verify loop.
//!
//! This module is the only place that sends signals or spawns new daemon processes.
//! It is intentionally synchronous: one daemon at a time.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::error::RolloutError;
use crate::fleet::DaemonRecipe;
use crate::health::poll_healthcheck;

/// Default healthcheck polling timeout.
pub(crate) const DEFAULT_HEALTHCHECK_TIMEOUT_SECS: u64 = 30;
/// Default healthcheck poll interval.
pub(crate) const DEFAULT_POLL_INTERVAL_MS: u64 = 500;

/// The result of a single daemon restart.
#[derive(Debug, Clone)]
pub(crate) struct RestartResult {
    /// Daemon name.
    pub name: String,
    /// PID before restart.
    pub old_pid: u32,
    /// PID after relaunch (if known).
    pub new_pid: Option<u32>,
    /// Unix timestamp (ms) when the restart step started.
    pub start_ms: u128,
    /// Unix timestamp (ms) when the restart step ended.
    pub end_ms: u128,
    /// Whether the restart succeeded (daemon re-registered).
    pub success: bool,
    /// Whether SIGKILL was required (process ignored SIGTERM).
    pub sigkill_used: bool,
}

/// Execute the full restart sequence for one daemon.
///
/// # Steps
///
/// 1. Run `build_cmd` in the recipe's repo dir (if set).
/// 2. Run `install_cmd`.
/// 3. Record the pre-restart peer set.
/// 4. Send SIGTERM to `old_pid`.
/// 5. Wait for the process to exit; send SIGKILL after `grace_period_secs`.
/// 6. Run `launch_cmd`.
/// 7. Poll the healthcheck until the daemon re-registers or times out.
///
/// # Errors
///
/// Returns an error on build/install failure, signal delivery failure, or
/// healthcheck timeout.
pub(crate) fn restart_daemon(
    recipe: &DaemonRecipe,
    old_pid: u32,
    healthcheck_timeout: Duration,
) -> Result<RestartResult, RolloutError> {
    let start_ms = now_ms();
    let mut sigkill_used = false;

    // Step 1: build
    run_recipe_cmd(&recipe.build_cmd, recipe.repo.as_deref(), &recipe.name, "build")?;

    // Step 2: install
    run_recipe_cmd(&recipe.install_cmd, recipe.repo.as_deref(), &recipe.name, "install")?;

    // Step 3+4: SIGTERM
    let pid = Pid::from_raw(i32::try_from(old_pid).unwrap_or(i32::MAX));
    kill(pid, Signal::SIGTERM)
        .map_err(|e| RolloutError::Signal(format!("SIGTERM {old_pid}: {e}")))?;

    eprintln!("rollout: sent SIGTERM to {name} pid={old_pid}", name = recipe.name);

    // Step 5: wait for exit
    let grace = Duration::from_secs(recipe.grace_period_secs);
    let waited = wait_for_exit(old_pid, grace);
    if !waited {
        eprintln!(
            "rollout: {name} pid={old_pid} did not exit after {g}s grace period; sending SIGKILL",
            name = recipe.name,
            g = recipe.grace_period_secs
        );
        let _ = kill(pid, Signal::SIGKILL); // best-effort
        sigkill_used = true;
        // Wait a moment for SIGKILL to take effect.
        std::thread::sleep(Duration::from_millis(200));
    }

    // Step 6: launch
    let new_pid = launch_daemon(recipe)?;

    // Step 7: healthcheck
    let hc = recipe.healthcheck_cmd();
    poll_healthcheck(
        &recipe.name,
        &hc,
        old_pid,
        new_pid,
        healthcheck_timeout,
        Duration::from_millis(DEFAULT_POLL_INTERVAL_MS),
    )?;

    let end_ms = now_ms();
    Ok(RestartResult {
        name: recipe.name.clone(),
        old_pid,
        new_pid,
        start_ms,
        end_ms,
        success: true,
        sigkill_used,
    })
}

/// Run a shell command in the given working directory (or current dir if `None`).
fn run_recipe_cmd(
    cmd: &str,
    workdir: Option<&std::path::Path>,
    daemon_name: &str,
    step: &str,
) -> Result<(), RolloutError> {
    let mut builder = std::process::Command::new("sh");
    builder.arg("-c").arg(cmd);
    if let Some(dir) = workdir {
        builder.current_dir(dir);
    }
    let status = builder
        .status()
        .map_err(|e| RolloutError::BuildFailed {
            name: daemon_name.to_owned(),
            reason: format!("{step} cmd could not be spawned: {e}"),
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(RolloutError::BuildFailed {
            name: daemon_name.to_owned(),
            reason: format!("{step} cmd exited {}", status.code().unwrap_or(-1)),
        })
    }
}

/// Launch a daemon via its `launch_cmd` and return its new PID if detectable.
fn launch_daemon(recipe: &DaemonRecipe) -> Result<Option<u32>, RolloutError> {
    let mut builder = std::process::Command::new("sh");
    builder.arg("-c").arg(&recipe.launch_cmd);
    if let Some(dir) = &recipe.repo {
        builder.current_dir(dir);
    }
    // Spawn detached; we'll get the PID from the healthcheck.
    let child = builder
        .spawn()
        .map_err(|e| RolloutError::LaunchFailed {
            name: recipe.name.clone(),
            reason: format!("spawn failed: {e}"),
        })?;
    let new_pid = Some(child.id());
    eprintln!(
        "rollout: launched {name} (pid={pid:?})",
        name = recipe.name,
        pid = new_pid
    );
    // Do NOT wait on child — it's a daemon that should outlive rollout.
    Ok(new_pid)
}

/// Poll for process exit by checking `/proc/<pid>` existence.
///
/// Returns `true` if the process exited within `timeout`, `false` otherwise.
fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let proc_path = format!("/proc/{pid}");
    loop {
        if !std::path::Path::new(&proc_path).exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Return the current time as Unix milliseconds.
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}
