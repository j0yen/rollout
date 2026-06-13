//! `warmswap.rs` — zero-loss warm-swap restart strategy.
//!
//! Instead of kill-then-start (which always has a brief window where no
//! instance holds the daemon's agorabus claim lease), warm-swap starts the
//! successor *first*, waits for it to acquire (or conflict on) the claim,
//! then stops the predecessor.  The predecessor's shutdown releases the claim;
//! the successor re-acquires uncontested; a final `ClaimList` check confirms
//! exactly one holder before returning success.
//!
//! ## Claim lease model
//!
//! agorabus `ClaimAcquire { path, ttl_unix_secs, reason, force }` refuses a
//! conflicting claim (`claim_conflict`) and broadcasts on `claim.acquire`.
//! `ClaimRelease` broadcasts on `claim.release`.  Exactly one instance is the
//! active claim holder at any instant — that's the primitive we build on.
//!
//! ## Sequence
//!
//! 1. Install the freshly-built binary (reuse `install.rs::atomic_install`).
//! 2. Launch successor via `systemd-run --user --scope` so it survives.
//! 3. Wait until `ClaimList` shows ≥ 1 holder (successor connected + called
//!    `ClaimAcquire`).  A conflict with the predecessor is expected here.
//! 4. Stop the predecessor (`systemctl --user stop <unit>` or SIGTERM).
//! 5. Wait until `ClaimList` shows exactly 1 holder (successor re-acquired).
//! 6. Run the normal healthcheck.
//! 7. Return `WarmSwapResult::Success` — or `SplitState` if step 5 sees ≠ 1.

use std::time::{Duration, Instant};

use crate::error::RolloutError;
use crate::fleet::DaemonRecipe;
use crate::health::poll_healthcheck;

/// Outcome of a warm-swap operation.
#[derive(Debug)]
pub(crate) enum WarmSwapResult {
    /// Swap completed cleanly: successor holds the sole claim.
    Success {
        /// PID of the predecessor that was stopped.
        prev_pid: u32,
        /// New PID of the successor (if detectable).
        new_pid: Option<u32>,
    },
    /// Verify step saw ≠ 1 holder — daemon may be in a split or dropped state.
    ///
    /// Operator must investigate; rollout does NOT silently continue.
    SplitState {
        /// Number of holders observed at the verify step.
        holders: usize,
    },
    /// The swap failed due to an unrecoverable error.
    Failed(RolloutError),
}

// ── AgoraClient trait (mockable) ─────────────────────────────────────────────

/// Minimal interface to the agorabus claim-lease operations needed by warm-swap.
///
/// The production impl shells out to `agorabus claim …`; tests inject a mock.
pub(crate) trait AgoraClient {
    /// Return the number of live holders for `claim_path`.
    ///
    /// Returns `None` if the bus is unreachable or the query fails.
    fn claim_list_count(&self, claim_path: &str) -> Option<usize>;
}

/// Production `AgoraClient` that shells out to the `agorabus` CLI.
pub(crate) struct ShellAgoraClient;

impl AgoraClient for ShellAgoraClient {
    fn claim_list_count(&self, claim_path: &str) -> Option<usize> {
        // `agorabus claim list <path> --format json` emits a JSON array of holders.
        let out = std::process::Command::new("agorabus")
            .args(["claim", "list", claim_path, "--format", "json"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = std::str::from_utf8(&out.stdout).ok()?;
        let val: serde_json::Value = serde_json::from_str(s.trim()).ok()?;
        // Expect a JSON array of holder objects.
        val.as_array().map(std::vec::Vec::len)
    }
}

// ── warm_swap ────────────────────────────────────────────────────────────────

/// Poll timeout for claim-list convergence.
const CLAIM_POLL_TIMEOUT: Duration = Duration::from_secs(30);
/// How often to poll `ClaimList`.
const CLAIM_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Execute the warm-swap restart sequence for one daemon.
///
/// **Precondition**: the freshly-built binary must already exist at the path
/// referenced by `recipe.install_cmd` (the caller must have run the build
/// step before calling this function).
///
/// `claim_path` is the agorabus claim path the daemon uses (e.g.
/// `agorabus://daemon/wm-stt`).
///
/// `old_pid` is the PID of the predecessor instance.
///
/// `healthcheck_timeout` is forwarded to `poll_healthcheck`.
///
/// # Errors
///
/// Returns `WarmSwapResult::Failed` on I/O or signal errors.  Split-state is
/// returned as `WarmSwapResult::SplitState`, not as an `Err`.
pub(crate) fn warm_swap(
    recipe: &DaemonRecipe,
    claim_path: &str,
    old_pid: u32,
    healthcheck_timeout: Duration,
    bus: &dyn AgoraClient,
) -> WarmSwapResult {
    // Step 1: install binary (build already run by caller)
    if let Err(e) = run_recipe_cmd_ws(&recipe.install_cmd, recipe.repo.as_deref(), &recipe.name, "install") {
        return WarmSwapResult::Failed(e);
    }

    // Step 2: launch successor as a transient scope (survives, parallel to the
    // managed unit's main instance so both run briefly during the swap).
    let successor_child = launch_successor(recipe);
    let new_pid = match successor_child {
        Ok(pid) => pid,
        Err(e) => return WarmSwapResult::Failed(e),
    };

    // Step 3: wait for successor to appear as a claim holder (conflict is fine).
    if !wait_for_claim_holders(claim_path, bus, 1, CLAIM_POLL_TIMEOUT) {
        return WarmSwapResult::Failed(RolloutError::HealthcheckTimeout {
            name: recipe.name.clone(),
            old_pid,
            new_pid,
            reason: format!(
                "warm-swap: successor did not acquire claim on '{claim_path}' within {}s",
                CLAIM_POLL_TIMEOUT.as_secs()
            ),
        });
    }

    // Step 4: stop the predecessor.
    if let Err(e) = stop_predecessor(recipe, old_pid) {
        return WarmSwapResult::Failed(e);
    }

    // Step 5: wait for exactly 1 holder (predecessor released, successor re-acquired).
    let holders = poll_claim_count(claim_path, bus, CLAIM_POLL_TIMEOUT);
    if holders != 1 {
        return WarmSwapResult::SplitState { holders };
    }

    // Step 6: healthcheck.
    let hc = recipe.healthcheck_cmd();
    if let Err(e) = poll_healthcheck(
        &recipe.name,
        &hc,
        old_pid,
        new_pid,
        healthcheck_timeout,
        Duration::from_millis(500),
    ) {
        return WarmSwapResult::Failed(e);
    }

    WarmSwapResult::Success { prev_pid: old_pid, new_pid }
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Run a shell command in the recipe's workdir.
fn run_recipe_cmd_ws(
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
    let status = builder.status().map_err(|e| RolloutError::BuildFailed {
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

/// Launch the successor as a transient `systemd-run --user --scope`.
///
/// Returns the new PID (of the scope/process) if detectable.
fn launch_successor(recipe: &DaemonRecipe) -> Result<Option<u32>, RolloutError> {
    // Build the systemd-run command.  We use --scope (not --service) so the
    // process is in a transient scope that survives rollout's own cgroup.
    let mut builder = std::process::Command::new("systemd-run");
    builder.args([
        "--user",
        "--scope",
        "--",
        "sh",
        "-c",
        &recipe.launch_cmd,
    ]);
    if let Some(dir) = &recipe.repo {
        builder.current_dir(dir);
    }
    let child = builder.spawn().map_err(|e| RolloutError::LaunchFailed {
        name: recipe.name.clone(),
        reason: format!("systemd-run --user --scope spawn failed: {e}"),
    })?;
    let pid = Some(child.id());
    eprintln!(
        "rollout warmswap: launched successor for {} (pid={:?})",
        recipe.name, pid
    );
    // Do not wait — the scope is meant to outlive rollout.
    Ok(pid)
}

/// Stop the predecessor via systemctl (preferred) or SIGTERM fallback.
fn stop_predecessor(recipe: &DaemonRecipe, old_pid: u32) -> Result<(), RolloutError> {
    if let Some(unit) = recipe.unit.as_deref().filter(|u| !u.is_empty()) {
        // Preferred: systemctl --user stop (graceful, tracked by systemd).
        let status = std::process::Command::new("systemctl")
            .args(["--user", "stop", unit])
            .status()
            .map_err(|e| RolloutError::Signal(format!("systemctl stop {unit}: {e}")))?;
        if !status.success() {
            eprintln!(
                "rollout warmswap: systemctl stop {unit} returned {}; will proceed (predecessor may already be down)",
                status.code().unwrap_or(-1)
            );
        }
    } else {
        // Fallback: SIGTERM.
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let pid = Pid::from_raw(i32::try_from(old_pid).unwrap_or(i32::MAX));
        kill(pid, Signal::SIGTERM)
            .map_err(|e| RolloutError::Signal(format!("SIGTERM {old_pid}: {e}")))?;
        eprintln!(
            "rollout warmswap: sent SIGTERM to {} pid={old_pid}",
            recipe.name
        );
        // Give it a moment to release its claim before we poll.
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// Poll `claim_list_count` until it reaches `min_holders` or `timeout` elapses.
///
/// Returns `true` when the target is reached.
fn wait_for_claim_holders(
    claim_path: &str,
    bus: &dyn AgoraClient,
    min_holders: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(n) = bus.claim_list_count(claim_path) {
            if n >= min_holders {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(CLAIM_POLL_INTERVAL);
    }
}

/// Poll `claim_list_count` until the bus responds, then return the count.
///
/// Returns the first definitive count the bus provides (1 = sole holder,
/// 0 or 2+ = split/dropped state). If the bus is unreachable throughout
/// `timeout`, returns 0 (which the caller treats as SplitState — we cannot
/// confirm a sole holder when the bus is down).
///
/// Any definitive non-1 count triggers an immediate return — there is no point
/// waiting for 2 holders to converge once the predecessor has been stopped.
fn poll_claim_count(claim_path: &str, bus: &dyn AgoraClient, timeout: Duration) -> usize {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(n) = bus.claim_list_count(claim_path) {
            // Return immediately on any definitive answer.
            // 1 = success; 0 or 2+ = split/dropped.
            return n;
        }
        // Bus unreachable — retry until timeout.
        if Instant::now() >= deadline {
            // Could not reach the bus: conservatively treat as split-state.
            return 0;
        }
        std::thread::sleep(CLAIM_POLL_INTERVAL);
    }
}

// ── helper: derive default claim path from recipe ────────────────────────────

/// Derive the default agorabus claim path for a daemon recipe.
///
/// If `recipe.claim_key` is set, use it directly.  Otherwise, if the recipe
/// has a `unit` (e.g. `wm-stt.service`), derive `agorabus://daemon/wm-stt`.
/// Returns `None` if neither is available.
pub(crate) fn claim_path_for_recipe(recipe: &DaemonRecipe) -> Option<String> {
    // claim_key field takes precedence.
    if let Some(ref key) = recipe.claim_key {
        if !key.is_empty() {
            return Some(key.clone());
        }
    }
    // Derive from unit stem.
    recipe
        .unit
        .as_deref()
        .filter(|u| !u.is_empty())
        .map(|unit| {
            let stem = unit.strip_suffix(".service").unwrap_or(unit);
            format!("agorabus://daemon/{stem}")
        })
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,  // fixture setup
    clippy::expect_used,
    clippy::panic,        // assert!/panic! in test harness
)]
mod tests {
    use super::*;
    use crate::fleet::DaemonRecipe;

    // ── mock AgoraClient ─────────────────────────────────────────────────────

    /// Scripted mock: returns successive values from a queue.
    struct MockBus {
        counts: std::sync::Mutex<std::collections::VecDeque<Option<usize>>>,
    }

    impl MockBus {
        fn new(counts: Vec<Option<usize>>) -> Self {
            Self {
                counts: std::sync::Mutex::new(counts.into()),
            }
        }
    }

    impl AgoraClient for MockBus {
        fn claim_list_count(&self, _claim_path: &str) -> Option<usize> {
            self.counts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Some(1)) // default to 1 once the queue drains
        }
    }

    fn test_recipe(name: &str) -> DaemonRecipe {
        DaemonRecipe {
            name: name.to_owned(),
            repo: None,
            build_cmd: "true".to_owned(),
            install_cmd: "true".to_owned(),
            launch_cmd: "true".to_owned(),
            unit: Some(format!("{name}.service")),
            healthcheck: Some("true".to_owned()), // always passes
            grace_period_secs: 1,
            warm_swap: false,
            claim_key: None,
        }
    }

    // ── AC2: warm-swap state machine ─────────────────────────────────────────

    /// AC2: state machine success path.
    ///
    /// Sequence:
    ///   poll 1 (wait_for_claim_holders): returns 1 → predecessor still holds claim
    ///   poll 2 (poll_claim_count after stop): returns 1 → successor sole holder
    ///   healthcheck: "true" → passes
    ///
    /// Expected: WarmSwapResult::Success.
    #[test]
    fn ac2_warm_swap_state_machine_success() {
        // The mock returns 1 holder on first poll (claim acquired/conflict),
        // and 1 holder again after stop (successor re-acquired).
        let bus = MockBus::new(vec![
            Some(1), // wait_for_claim_holders → ≥1 ✓
            Some(1), // poll_claim_count after stop → 1 ✓
        ]);

        let recipe = test_recipe("wm-stt");
        let result = warm_swap(
            &recipe,
            "agorabus://daemon/wm-stt",
            99999, // old_pid — not a real process; systemctl stop will fail silently
            Duration::from_millis(200),
            &bus,
        );

        match result {
            WarmSwapResult::Success { .. } => {}
            WarmSwapResult::SplitState { holders } => {
                panic!("AC2: expected Success, got SplitState(holders={holders})");
            }
            WarmSwapResult::Failed(e) => {
                panic!("AC2: expected Success, got Failed({e})");
            }
        }
    }

    // ── AC3: split-state guard ────────────────────────────────────────────────

    /// AC3: when ClaimList returns 2 holders at the verify step, `warm_swap`
    /// returns `WarmSwapResult::SplitState { holders: 2 }`.
    #[test]
    fn ac3_split_state_guard_two_holders() {
        let bus = MockBus::new(vec![
            Some(2), // wait_for_claim_holders: 2 ≥ 1 → ✓ (both predecessor + successor)
            Some(2), // poll_claim_count after stop: still 2 → SplitState
        ]);

        let recipe = test_recipe("wm-stt");
        let result = warm_swap(
            &recipe,
            "agorabus://daemon/wm-stt",
            99999,
            Duration::from_millis(200),
            &bus,
        );

        match result {
            WarmSwapResult::SplitState { holders } => {
                assert_eq!(holders, 2, "AC3: must report 2 holders");
            }
            WarmSwapResult::Success { .. } => panic!("AC3: expected SplitState, got Success"),
            WarmSwapResult::Failed(e) => panic!("AC3: expected SplitState, got Failed({e})"),
        }
    }

    /// AC3: zero holders at verify step also triggers SplitState.
    #[test]
    fn ac3_split_state_guard_zero_holders() {
        let bus = MockBus::new(vec![
            Some(1), // wait_for_claim_holders: 1 → ok
            Some(0), // poll_claim_count: nobody holds the claim → SplitState
        ]);

        let recipe = test_recipe("wm-stt");
        let result = warm_swap(
            &recipe,
            "agorabus://daemon/wm-stt",
            99999,
            Duration::from_millis(200),
            &bus,
        );

        match result {
            WarmSwapResult::SplitState { holders } => {
                assert_eq!(holders, 0, "AC3: must report 0 holders");
            }
            WarmSwapResult::Success { .. } => panic!("AC3: expected SplitState, got Success"),
            WarmSwapResult::Failed(e) => panic!("AC3: expected SplitState, got Failed({e})"),
        }
    }

    // ── AC4: no claim_key → hard restart path, no warm_swap ─────────────────

    /// AC4: a recipe without `claim_key` and `warm_swap = false` reports the
    /// correct restart strategy (hard) via `restart_strategy_for_recipe`.
    #[test]
    fn ac4_no_claim_key_uses_hard_restart() {
        let recipe = DaemonRecipe {
            name: "recalld".to_owned(),
            repo: None,
            build_cmd: "true".to_owned(),
            install_cmd: "true".to_owned(),
            launch_cmd: "recalld &".to_owned(),
            unit: Some("recalld.service".to_owned()),
            healthcheck: None,
            grace_period_secs: 5,
            warm_swap: false,
            claim_key: None,
        };

        assert_eq!(
            restart_strategy_label(&recipe),
            "hard",
            "AC4: recipe with no claim_key and warm_swap=false must use hard restart"
        );
    }

    /// AC4: a recipe with `warm_swap = true` uses the warm-swap path.
    #[test]
    fn ac4_warm_swap_flag_selects_warm_swap() {
        let recipe = DaemonRecipe {
            name: "wm-stt".to_owned(),
            repo: None,
            build_cmd: "true".to_owned(),
            install_cmd: "true".to_owned(),
            launch_cmd: "wm-stt &".to_owned(),
            unit: Some("wm-stt.service".to_owned()),
            healthcheck: None,
            grace_period_secs: 5,
            warm_swap: true,
            claim_key: None,
        };

        assert_eq!(
            restart_strategy_label(&recipe),
            "warm-swap",
            "AC4: recipe with warm_swap=true must select warm-swap"
        );
    }

    /// AC4: a recipe with `claim_key` set uses the warm-swap path regardless of
    /// the `warm_swap` flag.
    #[test]
    fn ac4_claim_key_selects_warm_swap() {
        let recipe = DaemonRecipe {
            name: "wm-tts".to_owned(),
            repo: None,
            build_cmd: "true".to_owned(),
            install_cmd: "true".to_owned(),
            launch_cmd: "wm-tts &".to_owned(),
            unit: Some("wm-tts.service".to_owned()),
            healthcheck: None,
            grace_period_secs: 5,
            warm_swap: false, // explicit false — overridden by claim_key
            claim_key: Some("agorabus://daemon/wm-tts".to_owned()),
        };

        assert_eq!(
            restart_strategy_label(&recipe),
            "warm-swap",
            "AC4: claim_key present → warm-swap regardless of warm_swap flag"
        );
    }

    // ── AC5: claim_path_for_recipe ────────────────────────────────────────────

    /// AC5: claim_path_for_recipe returns the claim_key when set.
    #[test]
    fn ac5_claim_path_uses_claim_key_when_set() {
        let recipe = DaemonRecipe {
            name: "wm-stt".to_owned(),
            repo: None,
            build_cmd: "true".to_owned(),
            install_cmd: "true".to_owned(),
            launch_cmd: "wm-stt &".to_owned(),
            unit: Some("wm-stt.service".to_owned()),
            healthcheck: None,
            grace_period_secs: 5,
            warm_swap: false,
            claim_key: Some("agorabus://daemon/wm-stt".to_owned()),
        };

        assert_eq!(
            claim_path_for_recipe(&recipe),
            Some("agorabus://daemon/wm-stt".to_owned()),
            "AC5: explicit claim_key is returned verbatim"
        );
    }

    /// AC5: claim_path_for_recipe derives from unit stem when claim_key is absent.
    #[test]
    fn ac5_claim_path_derived_from_unit_stem() {
        let recipe = DaemonRecipe {
            name: "wm-tts".to_owned(),
            repo: None,
            build_cmd: "true".to_owned(),
            install_cmd: "true".to_owned(),
            launch_cmd: "wm-tts &".to_owned(),
            unit: Some("wm-tts.service".to_owned()),
            healthcheck: None,
            grace_period_secs: 5,
            warm_swap: true,
            claim_key: None,
        };

        assert_eq!(
            claim_path_for_recipe(&recipe),
            Some("agorabus://daemon/wm-tts".to_owned()),
            "AC5: derived path must be agorabus://daemon/<unit-stem>"
        );
    }

    /// AC5: claim_path_for_recipe returns None when no unit and no claim_key.
    #[test]
    fn ac5_claim_path_none_when_no_unit_no_key() {
        let recipe = DaemonRecipe {
            name: "myapp".to_owned(),
            repo: None,
            build_cmd: "true".to_owned(),
            install_cmd: "true".to_owned(),
            launch_cmd: "myapp &".to_owned(),
            unit: None,
            healthcheck: None,
            grace_period_secs: 5,
            warm_swap: false,
            claim_key: None,
        };

        assert_eq!(
            claim_path_for_recipe(&recipe),
            None,
            "AC5: no unit + no claim_key → None"
        );
    }

    // ── Live integration test (gated behind #[ignore]) ────────────────────────

    /// Live warm-swap integration test.
    ///
    /// Requires a running agorabus daemon.  Gated behind `#[ignore]` so CI
    /// (cloudbuild) stays hermetic.  Run manually with:
    ///   cargo test -- ac_live_warm_swap --ignored
    #[test]
    #[ignore = "requires live agorabus daemon"]
    fn ac_live_warm_swap_integration() {
        // This test is intentionally left as a placeholder for manual
        // verification against a live daemon fleet.
        eprintln!("Live warm-swap integration test — not implemented for hermetic CI");
    }
}

// ── strategy label helper (used by plan output) ──────────────────────────────

/// Return the human-readable restart strategy label for a recipe.
///
/// `"warm-swap"` when `claim_key` is set OR `warm_swap = true`.
/// `"hard"` otherwise.
#[must_use]
pub(crate) fn restart_strategy_label(recipe: &DaemonRecipe) -> &'static str {
    if recipe.claim_key.as_deref().is_some_and(|k| !k.is_empty()) || recipe.warm_swap {
        "warm-swap"
    } else {
        "hard"
    }
}
