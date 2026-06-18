//! AC1 (MUST): `rollout plan` against a binstale JSON containing one `deleted-exe`
//! daemon prints that daemon and the exact build/install/launch commands from its
//! fleet.toml recipe, and mutates nothing (no process killed, no file written).
//!
//! Also covers:
//! - AC1 warm-swap: `rollout plan` shows `strategy: warm-swap` for a daemon with
//!   `warm_swap = true`, and `strategy: hard` for a daemon without it.
//! - AC5: `rollout plan --only wm-stt` with a warm-swap recipe prints the full ordered
//!   warm-swap sequence (install → launch successor → acquire → stop predecessor → verify).

use std::io::Write;
use std::process::Command;

use tempfile::TempDir;

/// Writes a minimal fleet.toml for a fixture daemon (no warm-swap).
fn write_fleet_toml(dir: &std::path::Path, name: &str, build: &str, install: &str, launch: &str) {
    let content = format!(
        "[[daemon]]\nname = \"{name}\"\nbuild_cmd = \"{build}\"\ninstall_cmd = \"{install}\"\nlaunch_cmd = \"{launch}\"\n"
    );
    std::fs::write(dir.join("fleet.toml"), content).expect("write fleet.toml");
}

/// Writes a fleet.toml for a warm-swap daemon with `warm_swap = true` and a unit.
fn write_fleet_toml_warmswap(dir: &std::path::Path, name: &str, unit: &str) {
    let content = format!(
        "[[daemon]]\nname = \"{name}\"\nbuild_cmd = \"true\"\ninstall_cmd = \"true\"\nlaunch_cmd = \"{name} &\"\nunit = \"{unit}\"\nwarm_swap = true\n"
    );
    std::fs::write(dir.join("fleet.toml"), content).expect("write fleet.toml warm-swap");
}

/// Writes a fleet.toml for a warm-swap daemon with an explicit `claim_key`.
fn write_fleet_toml_claim_key(dir: &std::path::Path, name: &str, unit: &str, claim_key: &str) {
    let content = format!(
        "[[daemon]]\nname = \"{name}\"\nbuild_cmd = \"true\"\ninstall_cmd = \"true\"\nlaunch_cmd = \"{name} &\"\nunit = \"{unit}\"\nclaim_key = \"{claim_key}\"\n"
    );
    std::fs::write(dir.join("fleet.toml"), content).expect("write fleet.toml claim-key");
}

/// Writes a binstale JSON with one deleted-exe entry.
fn binstale_json(name: &str, pid: u32) -> String {
    format!(
        r#"[{{"pid":{pid},"comm":"{name}","exe_path":"/usr/local/bin/{name} (deleted)","exe_inode":null,"ondisk_inode":null,"prov_ts":null,"proc_start":null,"verdict":"deleted-exe","evidence":{{"exe_deleted_suffix":true,"inode_mismatch":false,"timestamp_newer":false,"timestamp_source":"unavailable"}}}}]"#
    )
}

#[test]
fn plan_prints_daemon_and_commands_no_mutation() {
    let tmp = TempDir::new().expect("create tempdir");
    write_fleet_toml(
        tmp.path(),
        "fixture-daemon",
        "echo build",
        "echo install",
        "echo launch",
    );

    let json = binstale_json("fixture-daemon", 99999);
    let json_path = tmp.path().join("binstale.json");
    std::fs::write(&json_path, &json).expect("write json");

    // Snapshot files/processes before.
    let before_files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("readdir")
        .map(|e| e.expect("entry").path())
        .collect();

    let rollout_bin = env!("CARGO_BIN_EXE_rollout");
    let json_str = std::fs::read_to_string(&json_path).expect("read json");

    let output = Command::new(rollout_bin)
        .args([
            "plan",
            "--fleet",
            tmp.path().join("fleet.toml").to_str().expect("path"),
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
                .write_all(json_str.as_bytes())
                .expect("write stdin");
            child.wait_with_output()
        })
        .expect("run rollout plan");

    assert!(
        output.status.success(),
        "rollout plan should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("fixture-daemon"),
        "stdout should name the daemon; got: {stdout}"
    );
    assert!(
        stdout.contains("echo build"),
        "stdout should show build_cmd; got: {stdout}"
    );
    assert!(
        stdout.contains("echo install"),
        "stdout should show install_cmd; got: {stdout}"
    );
    assert!(
        stdout.contains("echo launch"),
        "stdout should show launch_cmd; got: {stdout}"
    );

    // Verify no new files were written.
    let after_files: Vec<_> = std::fs::read_dir(tmp.path())
        .expect("readdir")
        .map(|e| e.expect("entry").path())
        .collect();
    assert_eq!(
        before_files.len(),
        after_files.len(),
        "plan should not write any files"
    );
}

/// AC1 (warm-swap strategy column): `rollout plan` shows `strategy: hard` for a daemon
/// with no `warm_swap` flag and no `claim_key`.
#[test]
fn plan_shows_strategy_hard_for_plain_daemon() {
    let tmp = TempDir::new().expect("create tempdir");
    write_fleet_toml(
        tmp.path(),
        "fixture-daemon",
        "echo build",
        "echo install",
        "echo launch",
    );

    let json = binstale_json("fixture-daemon", 99999);
    let rollout_bin = env!("CARGO_BIN_EXE_rollout");

    let output = Command::new(rollout_bin)
        .args([
            "plan",
            "--fleet",
            tmp.path().join("fleet.toml").to_str().expect("path"),
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
                .expect("write stdin");
            child.wait_with_output()
        })
        .expect("run rollout plan");

    assert!(
        output.status.success(),
        "plan should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("strategy: hard"),
        "AC1: plan must show 'strategy: hard' for plain daemon; got: {stdout}"
    );
    assert!(
        !stdout.contains("strategy: warm-swap"),
        "AC1: plan must NOT show 'strategy: warm-swap' for plain daemon; got: {stdout}"
    );
}

/// AC1 (warm-swap strategy column): `rollout plan` shows `strategy: warm-swap` for a
/// daemon with `warm_swap = true`.
#[test]
fn plan_shows_strategy_warm_swap_for_warm_swap_daemon() {
    let tmp = TempDir::new().expect("create tempdir");
    write_fleet_toml_warmswap(tmp.path(), "wm-stt", "wm-stt.service");

    let json = binstale_json("wm-stt", 99999);
    let rollout_bin = env!("CARGO_BIN_EXE_rollout");

    let output = Command::new(rollout_bin)
        .args([
            "plan",
            "--fleet",
            tmp.path().join("fleet.toml").to_str().expect("path"),
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
                .expect("write stdin");
            child.wait_with_output()
        })
        .expect("run rollout plan");

    assert!(
        output.status.success(),
        "plan should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("strategy: warm-swap"),
        "AC1: plan must show 'strategy: warm-swap' for warm_swap=true daemon; got: {stdout}"
    );
    assert!(
        !stdout.contains("strategy: hard"),
        "AC1: plan must NOT show 'strategy: hard' for warm-swap daemon; got: {stdout}"
    );
}

/// AC1 (warm-swap strategy column): `rollout plan` shows `strategy: warm-swap` for a
/// daemon with an explicit `claim_key` (regardless of `warm_swap` flag).
#[test]
fn plan_shows_strategy_warm_swap_for_claim_key_daemon() {
    let tmp = TempDir::new().expect("create tempdir");
    write_fleet_toml_claim_key(
        tmp.path(),
        "wm-tts",
        "wm-tts.service",
        "agorabus://daemon/wm-tts",
    );

    let json = binstale_json("wm-tts", 99999);
    let rollout_bin = env!("CARGO_BIN_EXE_rollout");

    let output = Command::new(rollout_bin)
        .args([
            "plan",
            "--fleet",
            tmp.path().join("fleet.toml").to_str().expect("path"),
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
                .expect("write stdin");
            child.wait_with_output()
        })
        .expect("run rollout plan");

    assert!(
        output.status.success(),
        "plan should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("strategy: warm-swap"),
        "AC1: plan must show 'strategy: warm-swap' for daemon with claim_key; got: {stdout}"
    );
}

/// AC5 (plan --only warm-swap sequence): `rollout plan --only wm-stt` with a warm-swap
/// recipe prints the full ordered sequence without mutating anything.
///
/// Expected sequence lines:
///   1. install binary → dest
///   2. launch successor (systemd-run --user --scope)
///   3. wait for ClaimAcquire on '<claim_path>'
///   4. stop predecessor (systemctl --user stop)
///   5. verify exactly 1 holder on '<claim_path>'
#[test]
fn plan_only_warm_swap_prints_full_sequence_no_mutation() {
    let tmp = TempDir::new().expect("create tempdir");
    write_fleet_toml_warmswap(tmp.path(), "wm-stt", "wm-stt.service");

    let json = binstale_json("wm-stt", 99999);
    let rollout_bin = env!("CARGO_BIN_EXE_rollout");

    let output = Command::new(rollout_bin)
        .args([
            "plan",
            "--fleet",
            tmp.path().join("fleet.toml").to_str().expect("path"),
            "--from",
            "-",
            "--only",
            "wm-stt",
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
                .expect("write stdin");
            child.wait_with_output()
        })
        .expect("run rollout plan --only wm-stt");

    assert!(
        output.status.success(),
        "plan --only should exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // AC5: verify the warm-swap sequence is printed in order.
    assert!(
        stdout.contains("install binary"),
        "AC5: plan must print 'install binary' step; got: {stdout}"
    );
    assert!(
        stdout.contains("launch successor"),
        "AC5: plan must print 'launch successor' step; got: {stdout}"
    );
    assert!(
        stdout.contains("ClaimAcquire"),
        "AC5: plan must print 'ClaimAcquire' step; got: {stdout}"
    );
    assert!(
        stdout.contains("stop predecessor"),
        "AC5: plan must print 'stop predecessor' step; got: {stdout}"
    );
    assert!(
        stdout.contains("verify exactly 1 holder"),
        "AC5: plan must print 'verify exactly 1 holder' step; got: {stdout}"
    );

    // Verify the claim path is printed (derived from unit stem: agorabus://daemon/wm-stt).
    assert!(
        stdout.contains("agorabus://daemon/wm-stt"),
        "AC5: plan must show derived claim path; got: {stdout}"
    );

    // AC5: plan must not mutate anything — check the fleet.toml is unchanged.
    let fleet_content = std::fs::read_to_string(tmp.path().join("fleet.toml"))
        .expect("read fleet.toml after plan");
    assert!(
        fleet_content.contains("wm-stt"),
        "AC5: fleet.toml must be unchanged after plan; got: {fleet_content}"
    );
}
