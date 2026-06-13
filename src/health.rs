//! `health.rs` — agorabus peers poll / re-registration check.
//!
//! After relaunching a daemon, rollout polls the daemon's configured
//! healthcheck command until it exits 0 (success) or a deadline elapses.
//!
//! Two voice-set guards are implemented here:
//!
//! - **`--window` guard** (`check_window_guard`): coarse sample of
//!   `wm.dialog.turn.*` activity over a fixed window; opt-in via `--window`.
//! - **turn/session liveness probe** (`voice_activity_in_flight`): subscribes
//!   to `wm.dialog.turn` and `wm.brain.session` events and tracks in-flight
//!   state precisely; always applied to voice-set daemons in `apply`.

use std::io::BufRead as _;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::error::RolloutError;

/// Voice-set daemon name pattern.
///
/// Includes `wm-audio` (mic pipeline) in addition to `wm-dialog`, `wm-stt`,
/// and `wm-tts` — restarting any of these mid-turn drops the voice loop.
const VOICE_SET_PATTERN: &str = r"^wm-(audio|dialog|stt|tts)$";

/// Default sample duration for `voice_activity_in_flight`.
pub(crate) const DEFAULT_VOICE_SAMPLE_SECS: u64 = 3;

/// Idle window after the last `wm.dialog.turn.*` event before we consider
/// the turn complete (for the turn-level part of the in-flight check).
const TURN_IDLE_WINDOW: Duration = Duration::from_secs(2);

/// Result of the turn/session liveness probe.
#[derive(Debug)]
pub(crate) enum VoiceActivityState {
    /// A turn or session is actively in progress.
    InFlight {
        /// Human-readable reason for the in-flight determination.
        reason: String,
    },
    /// No activity detected within the sample window — safe to restart.
    Idle,
    /// The agorabus daemon could not be reached.
    ///
    /// Voice daemons should **defer** when the bus is unreachable: we cannot
    /// confirm the loop is idle.  Non-voice daemons may follow the existing
    /// conservative path (fail-open, proceed).
    BusUnreachable,
}

/// Static compiled regex for the voice-set pattern.
///
/// # Panics
///
/// Panics if `VOICE_SET_PATTERN` is not a valid regex — programmer error.
#[allow(clippy::panic)]
fn voice_set_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(VOICE_SET_PATTERN)
            .unwrap_or_else(|e| panic!("VOICE_SET_PATTERN compile error: {e}"))
    })
}

/// Return `true` if `name` is a voice-set daemon.
///
/// Voice-set: `wm-audio`, `wm-dialog`, `wm-stt`, `wm-tts`.
#[must_use]
pub(crate) fn is_voice_daemon(name: &str) -> bool {
    voice_set_regex().is_match(name)
}

/// Decide whether a voice daemon should be deferred based on activity state.
///
/// Returns `Some(reason)` when the daemon must not be restarted:
/// - `InFlight` → a turn or session is live.
/// - `BusUnreachable` → we cannot confirm the loop is idle.
///
/// Returns `None` when it is safe to proceed (`Idle`).
#[must_use]
pub(crate) fn voice_defer_reason(state: &VoiceActivityState) -> Option<String> {
    match state {
        VoiceActivityState::InFlight { reason } => {
            Some(format!("voice active: {reason}"))
        }
        VoiceActivityState::BusUnreachable => {
            Some("bus unreachable — cannot confirm voice loop is idle".to_owned())
        }
        VoiceActivityState::Idle => None,
    }
}

/// Subscribe to a bus topic prefix and stream lines to `tx` until EOF.
///
/// Sends `Some(line)` per received event and `None` on EOF.
/// Returns `Err` if the `agorabus subscribe` process could not be spawned.
fn subscribe_prefix(
    prefix: &str,
    tx: std::sync::mpsc::Sender<Option<String>>,
) -> Result<std::process::Child, std::io::Error> {
    let mut child = std::process::Command::new("agorabus")
        .args(["subscribe", prefix, "--no-reconnect"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "no stdout")
    })?;

    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            match line {
                Ok(l) => {
                    if tx.send(Some(l)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // EOF — signal the receiver.
        let _ = tx.send(None);
    });

    Ok(child)
}

/// Probe the agorabus bus for voice turn/session activity.
///
/// Subscribes to `wm.dialog.turn` and `wm.brain.session` for
/// `sample_duration`, tracking in-flight state from the event stream:
///
/// - `wm.brain.session.start` → in flight.
/// - `wm.brain.session.end` → clears session in-flight flag.
/// - `wm.dialog.turn.user` / `wm.dialog.turn.system` → in flight for
///   [`TURN_IDLE_WINDOW`] after the last event.
///
/// If both subscribers exit early with no events, returns [`VoiceActivityState::BusUnreachable`].
#[must_use]
pub(crate) fn voice_activity_in_flight(sample_duration: Duration) -> VoiceActivityState {
    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();

    // Number of subscriber threads; used to count EOF signals.
    const N_SUBS: u32 = 2;
    let prefixes = ["wm.dialog.turn", "wm.brain.session"];

    let mut children: Vec<std::process::Child> = Vec::new();
    for prefix in prefixes {
        match subscribe_prefix(prefix, tx.clone()) {
            Ok(child) => children.push(child),
            Err(_) => {
                // Spawn failed — kill any already-running subscribers and bail.
                for c in &mut children {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                return VoiceActivityState::BusUnreachable;
            }
        }
    }
    // Drop the original sender so the channel closes when both threads send None.
    drop(tx);

    let deadline = Instant::now() + sample_duration;
    let mut session_in_flight = false;
    let mut last_turn: Option<Instant> = None;
    let mut events_received: u32 = 0;
    let mut eof_count: u32 = 0;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(Some(line)) => {
                events_received = events_received.saturating_add(1);
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
                    if let Some(topic) = val.get("topic").and_then(|t| t.as_str()) {
                        match topic {
                            "wm.brain.session.start" => {
                                session_in_flight = true;
                            }
                            "wm.brain.session.end" => {
                                session_in_flight = false;
                            }
                            "wm.dialog.turn.user" | "wm.dialog.turn.system" => {
                                last_turn = Some(Instant::now());
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(None) => {
                eof_count = eof_count.saturating_add(1);
                if eof_count >= N_SUBS {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }

    // Both subscribers exited early with no events → could not connect.
    if events_received == 0 && eof_count >= N_SUBS {
        return VoiceActivityState::BusUnreachable;
    }

    if session_in_flight {
        return VoiceActivityState::InFlight {
            reason: "wm.brain.session.start seen without wm.brain.session.end".to_owned(),
        };
    }

    if let Some(t) = last_turn {
        if t.elapsed() < TURN_IDLE_WINDOW {
            return VoiceActivityState::InFlight {
                reason: format!(
                    "wm.dialog.turn.* seen {}ms ago (within {}ms idle window)",
                    t.elapsed().as_millis(),
                    TURN_IDLE_WINDOW.as_millis(),
                ),
            };
        }
    }

    VoiceActivityState::Idle
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
/// This is a coarse guard (fixed-window sample). For precise in-flight
/// detection, use [`voice_activity_in_flight`] instead.
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

    let Ok(mut child) = std::process::Command::new("agorabus")
        .args(["subscribe", "wm.dialog.turn.*"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        // If agorabus is not available, let the restart proceed (conservative
        // fail-open: if we can't check, we don't block).
        return Ok(());
    };

    std::thread::sleep(sample_duration);

    let _ = child.kill();
    let Ok(output) = child.wait_with_output() else {
        return Ok(()); // fail-open
    };

    if output.stdout.is_empty() {
        Ok(())
    } else {
        Err(RolloutError::WindowGuardBlocked {
            name: daemon_name.to_owned(),
        })
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    unsafe_code,         // set_var is unsafe in Rust 2024 — acceptable in single-threaded test code
    clippy::unwrap_used, // test helpers use expect/unwrap for fixture setup
    clippy::expect_used,
    clippy::panic,       // assert! expands to panic in test harness
)]
mod tests {
    use super::*;

    // ── AC4: voice set includes wm-audio ─────────────────────────────────────

    /// AC4: `wm-audio` is classified as a voice daemon.
    #[test]
    fn ac4_wm_audio_is_voice_daemon() {
        assert!(is_voice_daemon("wm-audio"), "AC4: wm-audio must be in the voice set");
    }

    /// AC4: the original three voice daemons are still in the set.
    #[test]
    fn ac4_original_voice_daemons_still_in_set() {
        for name in ["wm-dialog", "wm-stt", "wm-tts"] {
            assert!(is_voice_daemon(name), "AC4: {name} must be in the voice set");
        }
    }

    /// AC4: non-voice daemons are excluded.
    #[test]
    fn ac4_non_voice_daemons_excluded() {
        for name in ["recalld", "agorabus", "wm-brain", "wm-", "wm-audiox"] {
            assert!(!is_voice_daemon(name), "AC4: {name} must NOT be in the voice set");
        }
    }

    // ── AC1: VoiceActivityState enum and voice_activity_in_flight signature ──

    /// AC1 (structural): `VoiceActivityState` variants exist and are usable.
    #[test]
    fn ac1_voice_activity_state_variants() {
        // Verify all three arms compile and match correctly.
        let in_flight = VoiceActivityState::InFlight {
            reason: "test".to_owned(),
        };
        let idle = VoiceActivityState::Idle;
        let unreachable = VoiceActivityState::BusUnreachable;

        let mut in_flight_seen = false;
        let mut idle_seen = false;
        let mut unreachable_seen = false;

        for state in [in_flight, idle, unreachable] {
            match state {
                VoiceActivityState::InFlight { reason } => {
                    assert_eq!(reason, "test");
                    in_flight_seen = true;
                }
                VoiceActivityState::Idle => idle_seen = true,
                VoiceActivityState::BusUnreachable => unreachable_seen = true,
            }
        }
        assert!(in_flight_seen && idle_seen && unreachable_seen, "AC1: all variants covered");
    }

    /// AC1: `voice_activity_in_flight` with a minimal timeout returns without
    /// panicking.  We cannot guarantee a specific state here (depends on live
    /// bus), but the function must complete and return a valid variant.
    #[test]
    fn ac1_voice_activity_in_flight_completes() {
        let state = voice_activity_in_flight(Duration::from_millis(50));
        // Any valid state is acceptable; the test asserts no panic / hang.
        match state {
            VoiceActivityState::InFlight { .. }
            | VoiceActivityState::Idle
            | VoiceActivityState::BusUnreachable => {}
        }
    }

    // ── AC5: graceful degradation — bus unreachable returns BusUnreachable ───

    /// AC5 (structural): when `agorabus` is not on PATH, `subscribe_prefix` fails.
    ///
    /// We test this by overriding PATH to a directory without the binary and
    /// verifying that `voice_activity_in_flight` returns `BusUnreachable`.
    #[test]
    fn ac5_bus_unreachable_when_agorabus_absent() {
        // Override PATH so `agorabus` cannot be found.
        let old_path = std::env::var_os("PATH");
        // Safety: test-only, single-threaded at time of exec.
        unsafe {
            std::env::set_var("PATH", "/nonexistent");
        }
        let state = voice_activity_in_flight(Duration::from_millis(100));
        // Restore PATH immediately.
        match old_path {
            Some(p) => unsafe { std::env::set_var("PATH", p) },
            None => unsafe { std::env::remove_var("PATH") },
        }

        assert!(
            matches!(state, VoiceActivityState::BusUnreachable),
            "AC5: spawn failure must return BusUnreachable"
        );
    }

    // AC3 (session.start/end state machine) is covered by the binary-level
    // acceptance tests in tests/acceptance_window_guard_turnaware.rs, which
    // inject the fake agorabus via cmd.env("PATH", ...) — the safe, race-free
    // path.  Inline unit tests using set_var are excluded because parallel
    // test threads mutating the process environment is unsound in Rust 2024.
}
