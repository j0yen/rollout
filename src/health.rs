//! `health.rs` — agorabus peers poll / re-registration check.
//!
//! After relaunching a daemon, rollout polls the daemon's configured
//! healthcheck command until it exits 0 (success) or a deadline elapses.
//!
//! The `--window` guard is also implemented here: it runs a short
//! `agorabus subscribe` sample to detect recent `wm.dialog.turn.*`
//! activity before restarting voice-set daemons.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::error::RolloutError;

/// Voice-set daemon name pattern for the `--window` guard.
const VOICE_SET_PATTERN: &str = r"^wm-(dialog|stt|tts)$";

/// Static compiled regex for the voice-set pattern.
///
/// # Panics
///
/// Panics if `VOICE_SET_PATTERN` is not a valid regex — this is a programmer
/// error and can only happen if the constant is changed to an invalid pattern.
#[allow(clippy::panic)]
fn voice_set_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(VOICE_SET_PATTERN)
            .unwrap_or_else(|e| panic!("VOICE_SET_PATTERN compile error: {e}"))
    })
}

/// Check whether a daemon name is in the voice set.
#[must_use]
pub(crate) fn is_voice_daemon(name: &str) -> bool {
    voice_set_regex().is_match(name)
}

/// Poll the healthcheck command until it exits 0 or `timeout` elapses.
///
/// Returns `Ok(())` on success, or `Err(RolloutError::HealthcheckTimeout)` if
/// the deadline passes without the command succeeding.
///
/// # Errors
///
/// Returns a timeout error if the healthcheck does not pass within `timeout`.
pub(crate) fn poll_healthcheck(
    name: &str,
    cmd: &str,
    old_pid: u32,
    new_pid: Option<u32>,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<(), RolloutError> {
    let deadline = Instant::now() + timeout;
    loop {
        if run_shell_command(cmd) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(RolloutError::HealthcheckTimeout {
                name: name.to_owned(),
                old_pid,
                new_pid,
                reason: format!(
                    "healthcheck did not pass within {}s",
                    timeout.as_secs()
                ),
            });
        }
        std::thread::sleep(poll_interval);
    }
}

/// Run a shell command and return true if it exits 0.
fn run_shell_command(cmd: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The `--window` guard: sample the agorabus bus for `wm.dialog.turn.*`
/// events for `sample_duration`. Returns `Ok(())` if the bus is quiet
/// (no matching events), or `Err(WindowGuardBlocked)` if activity is seen.
///
/// # Errors
///
/// Returns a window guard error if voice activity is detected.
pub(crate) fn check_window_guard(
    daemon_name: &str,
    sample_duration: Duration,
) -> Result<(), RolloutError> {
    if !is_voice_daemon(daemon_name) {
        return Ok(());
    }

    // Sample: run `agorabus subscribe wm.dialog.turn.*` for sample_duration,
    // capture output. If any line appears, there's activity.
    let mut child = match std::process::Command::new("agorabus")
        .args(["subscribe", "wm.dialog.turn.*"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => {
            // If agorabus is not available, let the restart proceed (conservative
            // fail-open: if we can't check, we don't block).
            return Ok(());
        }
    };

    std::thread::sleep(sample_duration);

    // Kill the subscriber after sample_duration.
    let _ = child.kill();
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return Ok(()), // fail-open
    };

    // Any non-empty output means there was activity.
    let had_activity = !output.stdout.is_empty();
    if had_activity {
        Err(RolloutError::WindowGuardBlocked {
            name: daemon_name.to_owned(),
        })
    } else {
        Ok(())
    }
}
