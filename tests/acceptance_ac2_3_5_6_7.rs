//! Integration tests for the previously-unproven acceptance criteria.
//!
//! These are black-box tests: they drive the compiled `rollout` binary against
//! controlled fixture daemons (real `sleep` processes whose pids we own) and
//! shell-based build/install/launch/healthcheck recipes wired through a temp
//! directory. No part of the live fleet is touched.
//!
//! - AC2: `apply --only <fixture>` rebuilds, SIGTERMs the old pid, waits for
//!   exit, relaunches, and confirms a *different* new pid via healthcheck.
//! - AC3: `apply` processes stale daemons strictly one at a time — no two
//!   recipes are mid-execution (asserted via per-daemon start/ok output order
//!   plus a recipe-level mutex marker that would be tripped by any interleave).
//! - AC5: a daemon that fails to re-register within the healthcheck timeout
//!   stops the whole run (the next daemon is never started) and exits non-zero.
//! - AC6: SIGTERM is sent first; a SIGTERM-ignoring fixture is escalated to
//!   SIGKILL after the grace period, and the escalation is logged.
//! - AC7: `--window` refuses a voice-set daemon when `wm.dialog.turn.*`
//!   activity is observed on the (faked) bus, and allows it when the bus is
//!   quiet. A stub `agorabus` on PATH stands in for the live bus protocol.

// Test code legitimately uses `expect`/`unwrap` for fixture setup and `assert!`
// macros (which expand to `panic!`) for assertions; the crate's BAD_RUST
// restriction lints (`expect_used`, `unwrap_used`, `panic`) target production
// code, not the test harness. Allowing them here keeps this file clippy-clean
// under `--all-targets` without weakening the production crate's lint coverage.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::match_like_matches_macro,
    clippy::zombie_processes
)]

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const ROLLOUT_BIN: &str = env!("CARGO_BIN_EXE_rollout");

/// Spawn a long-lived fixture "daemon" (a plain `sleep`) and return its pid.
/// The process is owned by this test and is reaped/killed in the helper that
/// created it via `kill_pid` at teardown.
fn spawn_sleeper(secs: u64) -> u32 {
    let child = Command::new("sleep")
        .arg(secs.to_string())
        .spawn()
        .expect("spawn fixture sleeper");
    child.id()
}

/// Spawn a SIGTERM-ignoring fixture daemon. It traps TERM and keeps sleeping,
/// so only SIGKILL can take it down. Returns its pid.
fn spawn_term_ignoring_sleeper() -> u32 {
    // `exec sleep` would replace the shell and lose the trap, so we loop in the
    // shell itself with TERM trapped to a no-op.
    let child = Command::new("sh")
        .arg("-c")
        .arg("trap '' TERM; while true; do sleep 1; done")
        .spawn()
        .expect("spawn TERM-ignoring fixture");
    child.id()
}

/// Best-effort kill of a fixture pid (used for teardown).
fn kill_pid(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

/// Return true if the process is alive (running/sleeping), false if it is gone
/// **or a zombie**.
///
/// A fixture sleeper spawned by this test and then killed by `rollout` becomes a
/// `<defunct>` zombie until the test reaps it — and `/proc/<pid>` still exists
/// for a zombie. So mere directory existence is not "alive". We read the process
/// state from `/proc/<pid>/stat`; state `Z` (zombie) or `X` (dead) counts as not
/// alive, matching what `rollout`'s own `kill -0`-based logic considers exited.
fn pid_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false; // no such process
    };
    // stat format: "<pid> (<comm>) <state> ...". comm may contain spaces/parens,
    // so split on the last ')' and take the next non-space char as the state.
    let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start()) else {
        return false;
    };
    // Alive iff there is a state char and it is not zombie ('Z') or dead ('X'/'x').
    matches!(after_comm.chars().next(), Some(s) if !matches!(s, 'Z' | 'X' | 'x'))
}

/// Write a binstale JSON array with the given (name, pid) deleted-exe entries.
fn binstale_json(entries: &[(&str, u32)]) -> String {
    let objs: Vec<String> = entries
        .iter()
        .map(|(name, pid)| {
            format!(
                r#"{{"pid":{pid},"comm":"{name}","exe_path":"/usr/local/bin/{name} (deleted)","exe_inode":null,"ondisk_inode":null,"prov_ts":null,"proc_start":null,"verdict":"deleted-exe","evidence":{{"exe_deleted_suffix":true,"inode_mismatch":false,"timestamp_newer":false,"timestamp_source":"unavailable"}}}}"#
            )
        })
        .collect();
    format!("[{}]", objs.join(","))
}

/// Build a launch_cmd that starts a detached sleeper and records its pid in
/// `newpid_file`.
///
/// The sleeper is fully detached from `rollout`'s I/O **and inherited fds**: it
/// closes fds 3..=20 before backgrounding. This matters because `rollout` is
/// itself spawned by a shell that holds the build-skill lock on fd 9; without
/// closing it, the long-lived sleeper would inherit and pin that lock for 600s
/// (a test-harness artifact, not a product behavior — in production the daemon's
/// parent is systemd/init, not a lock-holding shell).
fn launch_sleeper_cmd(newpid_file: &Path) -> String {
    format!(
        "( for fd in 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do eval \"exec ${{fd}}>&- 2>/dev/null\"; done; \
         exec sleep 600 </dev/null >/dev/null 2>&1 ) & echo $! > {nf}",
        nf = newpid_file.display()
    )
}

/// Run `rollout` with the given args, feeding `binstale_stdin` on stdin.
/// Returns (success, stdout, stderr).
fn run_rollout(args: &[&str], binstale_stdin: &str) -> (bool, String, String) {
    let out = Command::new(ROLLOUT_BIN)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(binstale_stdin.as_bytes())
                .expect("write stdin");
            child.wait_with_output()
        })
        .expect("run rollout");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// ─── AC2 ─────────────────────────────────────────────────────────────────────

/// `apply --only <fixture>` against a real sleeper: rebuild (echo), SIGTERM the
/// old pid, wait for exit, relaunch (a new sleeper whose pid lands in a file the
/// healthcheck reads), and confirm the new pid differs from the old.
#[test]
fn apply_only_restarts_fixture_new_pid_differs() {
    let tmp = TempDir::new().expect("tempdir");
    let old_pid = spawn_sleeper(600);

    // launch_cmd: start a fresh sleeper, record its pid into newpid file.
    // healthcheck: succeed only once newpid exists AND names a live process.
    let newpid_file = tmp.path().join("newpid");
    let launch = launch_sleeper_cmd(&newpid_file);
    let health = format!(
        "test -s {nf} && kill -0 \"$(cat {nf})\"",
        nf = newpid_file.display()
    );

    let fleet = format!(
        "[[daemon]]\nname = \"fix-ac2\"\nbuild_cmd = \"echo build\"\ninstall_cmd = \"echo install\"\nlaunch_cmd = {launch:?}\nhealthcheck = {health:?}\ngrace_period_secs = 3\n"
    );
    let fleet_path = tmp.path().join("fleet.toml");
    std::fs::write(&fleet_path, fleet).expect("write fleet");

    let json = binstale_json(&[("fix-ac2", old_pid)]);
    let (ok, stdout, stderr) = run_rollout(
        &[
            "apply",
            "--fleet",
            fleet_path.to_str().unwrap(),
            "--from",
            "-",
            "--healthcheck-timeout",
            "15",
        ],
        &json,
    );

    assert!(ok, "apply should succeed; stdout={stdout}\nstderr={stderr}");
    // Old pid must have been terminated.
    assert!(
        !pid_alive(old_pid),
        "old fixture pid {old_pid} should have been SIGTERM'd"
    );
    // A new pid file must exist and name a different, live process.
    let new_pid: u32 = std::fs::read_to_string(&newpid_file)
        .expect("newpid written by launch_cmd")
        .trim()
        .parse()
        .expect("parse new pid");
    assert_ne!(new_pid, old_pid, "new pid must differ from old pid");
    assert!(pid_alive(new_pid), "relaunched daemon should be alive");
    assert!(
        stdout.contains("fix-ac2 ok") || stdout.contains("ok ("),
        "apply should report the daemon ok; stdout={stdout}"
    );

    kill_pid(new_pid);
    kill_pid(old_pid);
}

// ─── AC3 ─────────────────────────────────────────────────────────────────────

/// `apply` over two stale daemons runs them strictly serially. Each recipe's
/// launch_cmd writes `start <name>` then `end <name>` to a shared trace file
/// with a sleep in between. If execution ever interleaved, the trace would show
/// `start A`, `start B`, ...; a strict serial run yields fully nested, ordered
/// `start/end` pairs. We assert no interleave.
#[test]
fn apply_is_strictly_serialized_no_interleave() {
    let tmp = TempDir::new().expect("tempdir");
    let pid_a = spawn_sleeper(600);
    let pid_b = spawn_sleeper(600);

    let trace = tmp.path().join("trace.log");
    // Each daemon: launch records start/end around a short sleep; relaunch a
    // sleeper so the healthcheck (kill -0 on recorded pid) passes.
    let mk_launch = |name: &str| {
        let np = tmp.path().join(format!("newpid-{name}"));
        format!(
            "echo start {name} >> {tr}; sleep 0.5; \
             ( for fd in 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do eval \"exec ${{fd}}>&- 2>/dev/null\"; done; \
             exec sleep 600 </dev/null >/dev/null 2>&1 ) & echo $! > {np}; echo end {name} >> {tr}",
            tr = trace.display(),
            np = np.display()
        )
    };
    let mk_health = |name: &str| {
        let np = tmp.path().join(format!("newpid-{name}"));
        format!("test -s {np} && kill -0 \"$(cat {np})\"", np = np.display())
    };

    let fleet = format!(
        "[[daemon]]\nname = \"ser-a\"\ninstall_cmd = \"true\"\nbuild_cmd = \"true\"\nlaunch_cmd = {la:?}\nhealthcheck = {ha:?}\ngrace_period_secs = 3\n\n\
         [[daemon]]\nname = \"ser-b\"\ninstall_cmd = \"true\"\nbuild_cmd = \"true\"\nlaunch_cmd = {lb:?}\nhealthcheck = {hb:?}\ngrace_period_secs = 3\n",
        la = mk_launch("ser-a"),
        ha = mk_health("ser-a"),
        lb = mk_launch("ser-b"),
        hb = mk_health("ser-b"),
    );
    let fleet_path = tmp.path().join("fleet.toml");
    std::fs::write(&fleet_path, fleet).expect("write fleet");

    let json = binstale_json(&[("ser-a", pid_a), ("ser-b", pid_b)]);
    let (ok, stdout, stderr) = run_rollout(
        &[
            "apply",
            "--fleet",
            fleet_path.to_str().unwrap(),
            "--from",
            "-",
            "--healthcheck-timeout",
            "15",
        ],
        &json,
    );
    assert!(ok, "apply should succeed; stdout={stdout}\nstderr={stderr}");

    let trace_contents = std::fs::read_to_string(&trace).expect("trace written");
    let lines: Vec<&str> = trace_contents.lines().collect();
    // A strictly serial run yields exactly: start X, end X, start Y, end Y.
    assert_eq!(lines.len(), 4, "expected 4 trace lines, got: {lines:?}");
    assert!(
        lines[0].starts_with("start") && lines[1].starts_with("end"),
        "first recipe must complete before the second starts; trace={lines:?}"
    );
    // The end on line[1] must match the start on line[0] (same daemon nested).
    let first = lines[0].trim_start_matches("start ").trim();
    assert_eq!(
        lines[1].trim_start_matches("end ").trim(),
        first,
        "recipe must finish (end) before any other recipe starts; trace={lines:?}"
    );
    // Lines 2/3 are the second daemon, fully after the first.
    assert!(
        lines[2].starts_with("start") && lines[3].starts_with("end"),
        "second recipe should run after the first; trace={lines:?}"
    );

    // Reap relaunched sleepers.
    for n in ["ser-a", "ser-b"] {
        if let Ok(s) = std::fs::read_to_string(tmp.path().join(format!("newpid-{n}"))) {
            if let Ok(p) = s.trim().parse::<u32>() {
                kill_pid(p);
            }
        }
    }
    kill_pid(pid_a);
    kill_pid(pid_b);
}

// ─── AC5 ─────────────────────────────────────────────────────────────────────

/// A daemon whose healthcheck never passes must stop the run: the second daemon
/// is never started, and rollout exits non-zero reporting old/new pids.
#[test]
fn apply_stops_on_healthcheck_failure_does_not_cascade() {
    let tmp = TempDir::new().expect("tempdir");
    let pid_bad = spawn_sleeper(600);
    let pid_next = spawn_sleeper(600);

    // Marker the SECOND daemon's launch would create — it must NOT appear,
    // proving we never reached the second daemon.
    let next_marker = tmp.path().join("next-launched");

    let bad_launch = "sleep 600 </dev/null >/dev/null 2>&1 &"; // relaunches, but healthcheck below always fails
    let fleet = format!(
        "[[daemon]]\nname = \"bad\"\nbuild_cmd = \"true\"\ninstall_cmd = \"true\"\nlaunch_cmd = {bl:?}\nhealthcheck = \"false\"\ngrace_period_secs = 3\n\n\
         [[daemon]]\nname = \"next\"\nbuild_cmd = \"true\"\ninstall_cmd = \"true\"\nlaunch_cmd = \"touch {nm}\"\nhealthcheck = \"true\"\ngrace_period_secs = 3\n",
        bl = bad_launch,
        nm = next_marker.display(),
    );
    let fleet_path = tmp.path().join("fleet.toml");
    std::fs::write(&fleet_path, fleet).expect("write fleet");

    let json = binstale_json(&[("bad", pid_bad), ("next", pid_next)]);
    let (ok, stdout, stderr) = run_rollout(
        &[
            "apply",
            "--fleet",
            fleet_path.to_str().unwrap(),
            "--from",
            "-",
            "--healthcheck-timeout",
            "2",
        ],
        &json,
    );

    assert!(!ok, "apply must exit non-zero when a daemon fails healthcheck");
    assert!(
        !next_marker.exists(),
        "the second daemon must never be launched after the first fails"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("bad") && combined.to_lowercase().contains("health"),
        "failure must be reported with the daemon name and a healthcheck reason; got: {combined}"
    );
    assert!(
        combined.contains("stopping"),
        "rollout should announce it is stopping the run; got: {combined}"
    );

    kill_pid(pid_bad);
    kill_pid(pid_next);
}

// ─── AC6 ─────────────────────────────────────────────────────────────────────

/// A SIGTERM-ignoring fixture must be escalated to SIGKILL after the grace
/// period, the daemon must end up dead, and the escalation must be logged.
#[test]
fn sigterm_then_sigkill_on_uncooperative_daemon() {
    let tmp = TempDir::new().expect("tempdir");
    let stubborn_pid = spawn_term_ignoring_sleeper();

    // Give the trap a moment to install.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        pid_alive(stubborn_pid),
        "stubborn fixture should be alive at start"
    );

    let newpid_file = tmp.path().join("newpid");
    let launch = launch_sleeper_cmd(&newpid_file);
    let health = format!(
        "test -s {nf} && kill -0 \"$(cat {nf})\"",
        nf = newpid_file.display()
    );
    let fleet = format!(
        "[[daemon]]\nname = \"stubborn\"\nbuild_cmd = \"true\"\ninstall_cmd = \"true\"\nlaunch_cmd = {launch:?}\nhealthcheck = {health:?}\ngrace_period_secs = 1\n"
    );
    let fleet_path = tmp.path().join("fleet.toml");
    std::fs::write(&fleet_path, fleet).expect("write fleet");

    let json = binstale_json(&[("stubborn", stubborn_pid)]);
    let start = Instant::now();
    let (ok, stdout, stderr) = run_rollout(
        &[
            "apply",
            "--fleet",
            fleet_path.to_str().unwrap(),
            "--from",
            "-",
            "--healthcheck-timeout",
            "15",
        ],
        &json,
    );
    let elapsed = start.elapsed();

    assert!(ok, "apply should succeed after SIGKILL fallback; stdout={stdout}\nstderr={stderr}");
    // The stubborn process must be dead (SIGKILL cannot be trapped).
    // Allow a brief settle for the kernel to reap.
    let mut dead = false;
    for _ in 0..20 {
        if !pid_alive(stubborn_pid) {
            dead = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(dead, "SIGTERM-ignoring fixture must be killed via SIGKILL");
    // The grace period (1s) must have elapsed before the kill — proves SIGTERM
    // was tried first rather than an immediate SIGKILL.
    assert!(
        elapsed >= Duration::from_secs(1),
        "grace period should elapse before SIGKILL; elapsed={elapsed:?}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("SIGTERM"),
        "SIGTERM should be logged first; got: {combined}"
    );
    assert!(
        combined.contains("SIGKILL"),
        "SIGKILL escalation should be logged; got: {combined}"
    );

    if let Ok(s) = std::fs::read_to_string(&newpid_file) {
        if let Ok(p) = s.trim().parse::<u32>() {
            kill_pid(p);
        }
    }
    kill_pid(stubborn_pid);
}

// ─── AC7 ─────────────────────────────────────────────────────────────────────

/// Write a stub `agorabus` script into `dir/bin/agorabus` that, for
/// `subscribe wm.dialog.turn.*`, emits `body` on stdout (and stays alive long
/// enough to be sampled). Returns the bin dir to prepend to PATH.
fn write_agorabus_stub(dir: &Path, subscribe_emits: &str) -> std::path::PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir bin");
    let script = bin.join("agorabus");
    // If invoked as `agorabus subscribe ...`, optionally print activity then
    // block (so rollout's sampler can read it within the window). Any other
    // invocation (e.g. peers healthcheck) just exits 0.
    let body = format!(
        "#!/bin/sh\nif [ \"$1\" = \"subscribe\" ]; then\n{maybe}\n  # stay alive so the sampler can read before killing us; exec replaces this\n  # shell so the sleep is the direct child rollout kills (no fd leak)\n  exec sleep 10\nfi\nexit 0\n",
        maybe = if subscribe_emits.is_empty() {
            String::from("  :")
        } else {
            format!("  printf '%s\\n' '{subscribe_emits}'")
        }
    );
    std::fs::write(&script, body).expect("write stub");
    let mut perms = std::fs::metadata(&script).expect("meta").permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod");
    bin
}

/// Build a fleet with a single voice-set daemon (`wm-dialog`) whose healthcheck
/// always passes, so the only thing that can block apply is the window guard.
fn voice_fleet(tmp: &Path, newpid_file: &Path) -> std::path::PathBuf {
    let launch = launch_sleeper_cmd(newpid_file);
    let health = format!(
        "test -s {nf} && kill -0 \"$(cat {nf})\"",
        nf = newpid_file.display()
    );
    let fleet = format!(
        "[[daemon]]\nname = \"wm-dialog\"\nbuild_cmd = \"true\"\ninstall_cmd = \"true\"\nlaunch_cmd = {launch:?}\nhealthcheck = {health:?}\ngrace_period_secs = 3\n"
    );
    let p = tmp.join("fleet.toml");
    std::fs::write(&p, fleet).expect("write fleet");
    p
}

/// Run rollout with a modified PATH so our stub `agorabus` is found first.
fn run_rollout_with_path(
    args: &[&str],
    binstale_stdin: &str,
    extra_path_dir: &Path,
) -> (bool, String, String) {
    let orig = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{orig}", extra_path_dir.display());
    let out = Command::new(ROLLOUT_BIN)
        .args(args)
        .env("PATH", new_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(binstale_stdin.as_bytes())
                .expect("write stdin");
            child.wait_with_output()
        })
        .expect("run rollout");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `--window` must REFUSE a voice-set daemon when the bus shows
/// `wm.dialog.turn.*` activity (stub emits a turn event), exiting non-zero and
/// without ever launching the daemon.
#[test]
fn window_guard_blocks_voice_daemon_when_bus_active() {
    let tmp = TempDir::new().expect("tempdir");
    let bin_dir = write_agorabus_stub(tmp.path(), "wm.dialog.turn.start {}");
    let pid = spawn_sleeper(600);
    let newpid_file = tmp.path().join("newpid");
    let fleet_path = voice_fleet(tmp.path(), &newpid_file);

    let json = binstale_json(&[("wm-dialog", pid)]);
    let (ok, stdout, stderr) = run_rollout_with_path(
        &[
            "apply",
            "--fleet",
            fleet_path.to_str().unwrap(),
            "--from",
            "-",
            "--window",
            "1s",
            "--healthcheck-timeout",
            "10",
        ],
        &json,
        &bin_dir,
    );

    assert!(
        !ok,
        "window guard must block (non-zero) on active bus; stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        !newpid_file.exists(),
        "voice daemon must NOT be launched while the bus is active"
    );
    assert!(pid_alive(pid), "blocked daemon must not be killed");
    let combined = format!("{stdout}{stderr}").to_lowercase();
    assert!(
        combined.contains("window guard") || combined.contains("activity"),
        "blocked run should explain the window guard; got: {combined}"
    );

    kill_pid(pid);
}

/// `--window` must ALLOW a voice-set daemon when the bus is quiet (stub emits
/// nothing for the sample window): the daemon is restarted and the run succeeds.
#[test]
fn window_guard_allows_voice_daemon_when_bus_quiet() {
    let tmp = TempDir::new().expect("tempdir");
    let bin_dir = write_agorabus_stub(tmp.path(), ""); // quiet: emits nothing
    let pid = spawn_sleeper(600);
    let newpid_file = tmp.path().join("newpid");
    let fleet_path = voice_fleet(tmp.path(), &newpid_file);

    let json = binstale_json(&[("wm-dialog", pid)]);
    let (ok, stdout, stderr) = run_rollout_with_path(
        &[
            "apply",
            "--fleet",
            fleet_path.to_str().unwrap(),
            "--from",
            "-",
            "--window",
            "1s",
            "--healthcheck-timeout",
            "10",
        ],
        &json,
        &bin_dir,
    );

    assert!(
        ok,
        "window guard must allow restart when bus is quiet; stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        !pid_alive(pid),
        "old pid should have been restarted when the window allowed it"
    );
    assert!(
        newpid_file.exists(),
        "voice daemon should have been launched when the bus is quiet"
    );

    if let Ok(s) = std::fs::read_to_string(&newpid_file) {
        if let Ok(p) = s.trim().parse::<u32>() {
            kill_pid(p);
        }
    }
    kill_pid(pid);
}
