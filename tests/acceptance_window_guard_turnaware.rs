//! Acceptance tests for `rollout-window-guard-turnaware`.
//!
//! Tests for the turn/session liveness probe:
//!
//! - AC2: with a simulated in-flight session, `apply` against a voice daemon
//!   **defers** (no restart) and exits non-zero.
//! - AC3: with no activity, the same `apply` proceeds normally.
//! - AC4: `wm-audio` is in the voice set (binary-level smoke).
//! - AC5: bus-unreachable causes a voice-daemon deferral, not a blind restart.
//! - AC6: deferral is reported in stdout; run exits non-zero so self-review
//!   can detect it without false "refreshed" claim.
//!
//! A stub `agorabus` is placed on PATH to control what events the probe sees.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::zombie_processes
)]

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const ROLLOUT_BIN: &str = env!("CARGO_BIN_EXE_rollout");

// ── helpers ───────────────────────────────────────────────────────────────────

fn spawn_sleeper(secs: u64) -> u32 {
    Command::new("sleep")
        .arg(secs.to_string())
        .spawn()
        .expect("spawn sleeper")
        .id()
}

fn kill_pid(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

/// Binstale JSON for a single (name, pid) "deleted-exe" entry.
fn binstale_json_one(name: &str, pid: u32) -> String {
    format!(
        r#"[{{"pid":{pid},"comm":"{name}","exe_path":"/usr/local/bin/{name} (deleted)","exe_inode":null,"ondisk_inode":null,"prov_ts":null,"proc_start":null,"verdict":"deleted-exe","evidence":{{"exe_deleted_suffix":true,"inode_mismatch":false,"timestamp_newer":false,"timestamp_source":"unavailable"}}}}]"#
    )
}

/// Write a fake `agorabus` stub to `dir` that emits `events_json` lines when
/// `subscribe` is called, then sleeps.
///
/// `events_json` is a list of raw JSON strings to emit on stdout, one per
/// `subscribe` arg that matches `match_prefix`.  If `match_prefix` is empty,
/// events are emitted unconditionally.
fn write_fake_agorabus(dir: &Path, session_start: bool, session_end: bool) {
    let fake = dir.join("agorabus");

    // Emit session.start (and optionally session.end) when subscribe is called
    // with the brain.session prefix.  Other subscribe calls get nothing.
    let start_line = if session_start {
        r#"printf '{"topic":"wm.brain.session.start","data":{},"from":"test"}\n'"#
    } else {
        "true"
    };
    let end_line = if session_end {
        r#"printf '{"topic":"wm.brain.session.end","data":{},"from":"test"}\n'"#
    } else {
        "true"
    };

    let script = format!(
        r#"#!/bin/sh
# Fake agorabus for rollout tests.
CMD="${{1:-}}"
if [ "$CMD" = "subscribe" ]; then
  PREFIX="${{2:-}}"
  case "$PREFIX" in
    *brain.session*)
      {start_line}
      {end_line}
      ;;
    *)
      ;;
  esac
  # Hang briefly to simulate an open subscription.
  sleep 5
fi
"#
    );
    std::fs::write(&fake, &script).expect("write fake agorabus");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
        .expect("chmod fake agorabus");
}

/// Write a fake `agorabus` that exits immediately with no output (simulates
/// unreachable bus: --no-reconnect → spawn OK but EOF immediately).
fn write_fake_agorabus_unreachable(dir: &Path) {
    let fake = dir.join("agorabus");
    let script = "#!/bin/sh\n# Fake unreachable agorabus: exit immediately.\nexit 1\n";
    std::fs::write(&fake, script).expect("write fake unreachable agorabus");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
}

/// Build a fleet.toml entry for a voice daemon that launches a new sleeper.
fn fleet_with_voice_daemon(tmp: &Path, daemon_name: &str) -> std::path::PathBuf {
    let newpid = tmp.join("newpid");
    let launched_marker = tmp.join("launched");
    // Write a ready marker and our PID, then exec sleep.  Using $$ (current
    // shell PID) avoids $! races: the file is written synchronously in the
    // subshell before exec replaces it with sleep, so when the healthcheck
    // sees the marker the PID is guaranteed to exist in /proc.
    let launch = format!(
        "( echo $$ > {nf}; touch {lm}; exec sleep 600 </dev/null >/dev/null 2>&1 ) &",
        nf = newpid.display(),
        lm = launched_marker.display(),
    );
    // Healthcheck: marker exists (launch ran) and /proc/<pid> is present.
    let health = format!(
        "test -f {lm} && test -s {nf} && test -d /proc/\"$(cat {nf})\"",
        lm = launched_marker.display(),
        nf = newpid.display(),
    );
    let fleet = format!(
        "[[daemon]]\nname = \"{daemon_name}\"\nbuild_cmd = \"echo build\"\ninstall_cmd = \"echo install\"\nlaunch_cmd = {launch:?}\nhealthcheck = {health:?}\ngrace_period_secs = 3\n"
    );
    let fleet_path = tmp.join("fleet.toml");
    std::fs::write(&fleet_path, fleet).expect("write fleet");
    fleet_path
}

/// Run `rollout apply` with the given extra env and fleet.
fn run_apply_with_env(
    fleet_path: &Path,
    json_stdin: &str,
    extra_path: Option<&str>,
) -> (bool, String, String) {
    let mut cmd = Command::new(ROLLOUT_BIN);
    cmd.args([
        "apply",
        "--fleet",
        fleet_path.to_str().unwrap(),
        "--from",
        "-",
        "--healthcheck-timeout",
        "5",
    ])
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped());

    if let Some(prepend) = extra_path {
        let base = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{prepend}:{base}"));
    }

    let out = cmd
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(json_stdin.as_bytes())
                .expect("write");
            child.wait_with_output()
        })
        .expect("run rollout");

    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// ── AC2: in-flight session → defer, no restart ────────────────────────────────

/// AC2: when `wm.brain.session.start` is seen (no matching `session.end`),
/// `apply` defers the voice daemon — does NOT send it a SIGTERM — and exits
/// non-zero.
///
/// We verify that the old sleeper is still alive after the apply run.
#[test]
fn ac2_session_in_flight_defers_voice_daemon() {
    let tmp = TempDir::new().expect("tempdir");
    let old_pid = spawn_sleeper(600);

    // Fake agorabus: emit session.start only (no session.end → in flight).
    write_fake_agorabus(tmp.path(), true, false);
    let fleet_path = fleet_with_voice_daemon(tmp.path(), "wm-dialog");
    let json = binstale_json_one("wm-dialog", old_pid);

    let (ok, stdout, _stderr) = run_apply_with_env(
        &fleet_path,
        &json,
        Some(tmp.path().to_str().unwrap()),
    );

    // Old process must still be alive (no SIGTERM was sent).
    let alive = std::path::Path::new(&format!("/proc/{old_pid}")).exists();
    kill_pid(old_pid);

    assert!(!ok, "AC2: apply must exit non-zero when voice daemon is deferred");
    assert!(alive, "AC2: voice daemon old pid must still be alive (not restarted)");
    assert!(
        stdout.contains("deferred"),
        "AC2: stdout must contain 'deferred'; got: {stdout}"
    );
}

// ── AC3: idle bus → apply proceeds normally ──────────────────────────────────

/// AC3: when no turn/session events are seen (session.start then session.end =
/// idle), `apply` proceeds and restarts the daemon normally.
#[test]
fn ac3_idle_session_allows_restart() {
    let tmp = TempDir::new().expect("tempdir");
    let old_pid = spawn_sleeper(600);

    // Fake agorabus: emit session.start then session.end → idle.
    write_fake_agorabus(tmp.path(), true, true);
    let fleet_path = fleet_with_voice_daemon(tmp.path(), "wm-tts");
    let json = binstale_json_one("wm-tts", old_pid);

    let (ok, stdout, stderr) = run_apply_with_env(
        &fleet_path,
        &json,
        Some(tmp.path().to_str().unwrap()),
    );

    kill_pid(old_pid); // cleanup even if alive

    assert!(
        ok,
        "AC3: apply must succeed when bus is idle; stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("ok") || stdout.contains("restarted"),
        "AC3: stdout must indicate restart ok; got: {stdout}"
    );
    assert!(
        !stdout.contains("deferred"),
        "AC3: no deferral should be reported when idle; got: {stdout}"
    );
}

// ── AC4: wm-audio is in the voice set (binary smoke) ─────────────────────────

/// AC4 (binary smoke): running `rollout plan` with a stale `wm-audio` entry
/// in fleet.toml does NOT crash; the binary accepts wm-audio as a known daemon.
///
/// The deeper unit-level AC4 test lives in `health.rs`; this test ensures the
/// voice-set expansion is visible end-to-end through the binary.
#[test]
fn ac4_wm_audio_accepted_in_plan() {
    let tmp = TempDir::new().expect("tempdir");
    let fleet = r#"[[daemon]]
name = "wm-audio"
build_cmd = "echo build"
install_cmd = "echo install"
launch_cmd = "echo launch"
grace_period_secs = 3
"#;
    let fleet_path = tmp.path().join("fleet.toml");
    std::fs::write(&fleet_path, fleet).expect("write fleet");

    let json = binstale_json_one("wm-audio", 99999);
    let out = Command::new(ROLLOUT_BIN)
        .args([
            "plan",
            "--fleet",
            fleet_path.to_str().unwrap(),
            "--from",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(json.as_bytes())
                .expect("write");
            child.wait_with_output()
        })
        .expect("run rollout plan");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "AC4: rollout plan with wm-audio must succeed; stdout={stdout}"
    );
    assert!(
        stdout.contains("wm-audio"),
        "AC4: wm-audio should appear in plan output; got: {stdout}"
    );
}

// ── AC5: bus unreachable → voice daemon deferred ─────────────────────────────

/// AC5: when agorabus is unreachable (stub exits immediately, no events),
/// the voice daemon is deferred rather than restarted blind.
#[test]
fn ac5_bus_unreachable_defers_voice_daemon() {
    let tmp = TempDir::new().expect("tempdir");
    let old_pid = spawn_sleeper(600);

    write_fake_agorabus_unreachable(tmp.path());
    let fleet_path = fleet_with_voice_daemon(tmp.path(), "wm-stt");
    let json = binstale_json_one("wm-stt", old_pid);

    let (ok, stdout, _stderr) = run_apply_with_env(
        &fleet_path,
        &json,
        Some(tmp.path().to_str().unwrap()),
    );

    let alive = std::path::Path::new(&format!("/proc/{old_pid}")).exists();
    kill_pid(old_pid);

    assert!(!ok, "AC5: apply must exit non-zero when bus unreachable defers voice daemon");
    assert!(alive, "AC5: old pid must still be alive (not restarted blind)");
    assert!(
        stdout.contains("deferred"),
        "AC5: stdout must contain 'deferred'; got: {stdout}"
    );
}

// ── AC6: deferral reported, exits non-zero ────────────────────────────────────

/// AC6: the deferred daemons are listed in stdout and the exit code is non-zero,
/// so `rollout apply` does not falsely claim all daemons were refreshed.
#[test]
fn ac6_deferred_exit_nonzero_and_reported() {
    let tmp = TempDir::new().expect("tempdir");
    let old_pid = spawn_sleeper(600);

    // In-flight session → both wm-dialog and wm-tts should be deferred.
    write_fake_agorabus(tmp.path(), true, false);

    // Fleet with two voice daemons.
    let newpid = tmp.path().join("newpid");
    let launch = format!(
        "sleep 600 & echo $! > {}",
        newpid.display()
    );
    let health = format!("test -s {}", newpid.display());
    let fleet = format!(
        "[[daemon]]\nname = \"wm-dialog\"\nbuild_cmd = \"echo b\"\ninstall_cmd = \"echo i\"\nlaunch_cmd = {launch:?}\nhealthcheck = {health:?}\ngrace_period_secs = 3\n\
         [[daemon]]\nname = \"wm-tts\"\nbuild_cmd = \"echo b\"\ninstall_cmd = \"echo i\"\nlaunch_cmd = {launch:?}\nhealthcheck = {health:?}\ngrace_period_secs = 3\n"
    );
    let fleet_path = tmp.path().join("fleet.toml");
    std::fs::write(&fleet_path, fleet).expect("write fleet");

    // Two voice daemons in the stale list.
    let old_pid2 = spawn_sleeper(600);
    let json = format!(
        r#"[{{"pid":{old_pid},"comm":"wm-dialog","exe_path":"/usr/local/bin/wm-dialog (deleted)","exe_inode":null,"ondisk_inode":null,"prov_ts":null,"proc_start":null,"verdict":"deleted-exe","evidence":{{"exe_deleted_suffix":true,"inode_mismatch":false,"timestamp_newer":false,"timestamp_source":"unavailable"}}}},{{"pid":{old_pid2},"comm":"wm-tts","exe_path":"/usr/local/bin/wm-tts (deleted)","exe_inode":null,"ondisk_inode":null,"prov_ts":null,"proc_start":null,"verdict":"deleted-exe","evidence":{{"exe_deleted_suffix":true,"inode_mismatch":false,"timestamp_newer":false,"timestamp_source":"unavailable"}}}}]"#
    );

    let (ok, stdout, _stderr) = run_apply_with_env(
        &fleet_path,
        &json,
        Some(tmp.path().to_str().unwrap()),
    );

    kill_pid(old_pid);
    kill_pid(old_pid2);

    assert!(!ok, "AC6: apply must exit non-zero when daemons are deferred");
    assert!(
        stdout.contains("deferred"),
        "AC6: stdout must report deferral; got: {stdout}"
    );
    // The output must name at least one of the two deferred daemons.
    assert!(
        stdout.contains("wm-dialog") || stdout.contains("wm-tts"),
        "AC6: deferred daemon names must appear in stdout; got: {stdout}"
    );
}
