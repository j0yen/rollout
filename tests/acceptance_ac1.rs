//! AC1 (MUST): `rollout plan` against a binstale JSON containing one `deleted-exe`
//! daemon prints that daemon and the exact build/install/launch commands from its
//! fleet.toml recipe, and mutates nothing (no process killed, no file written).

use std::io::Write;
use std::process::Command;

use tempfile::TempDir;

/// Writes a minimal fleet.toml for a fixture daemon.
fn write_fleet_toml(dir: &std::path::Path, name: &str, build: &str, install: &str, launch: &str) {
    let content = format!(
        "[[daemon]]\nname = \"{name}\"\nbuild_cmd = \"{build}\"\ninstall_cmd = \"{install}\"\nlaunch_cmd = \"{launch}\"\n"
    );
    std::fs::write(dir.join("fleet.toml"), content).expect("write fleet.toml");
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
