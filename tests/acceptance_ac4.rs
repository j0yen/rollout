//! AC4 (MUST): A daemon with no entry in fleet.toml is refused with a clear error
//! and is never killed; `rollout` exits non-zero listing the unknown daemons.

use std::io::Write;
use std::process::Command;

use tempfile::TempDir;

fn empty_fleet_toml(dir: &std::path::Path) {
    // An empty fleet config (no [[daemon]] entries).
    std::fs::write(dir.join("fleet.toml"), "").expect("write fleet.toml");
}

fn binstale_json_unknown(name: &str, pid: u32) -> String {
    format!(
        r#"[{{"pid":{pid},"comm":"{name}","exe_path":"/usr/local/bin/{name} (deleted)","exe_inode":null,"ondisk_inode":null,"prov_ts":null,"proc_start":null,"verdict":"deleted-exe","evidence":{{"exe_deleted_suffix":true,"inode_mismatch":false,"timestamp_newer":false,"timestamp_source":"unavailable"}}}}]"#
    )
}

#[test]
fn apply_refuses_unknown_daemon_exits_nonzero() {
    let tmp = TempDir::new().expect("create tempdir");
    empty_fleet_toml(tmp.path());

    let json = binstale_json_unknown("ghost-daemon", 99998);
    let rollout_bin = env!("CARGO_BIN_EXE_rollout");

    let output = Command::new(rollout_bin)
        .args([
            "apply",
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
        .expect("run rollout apply");

    assert!(
        !output.status.success(),
        "rollout apply should exit non-zero for unknown daemon"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("ghost-daemon"),
        "output should name the unknown daemon; stderr={stderr}"
    );
}

#[test]
fn plan_refuses_unknown_daemon_exits_nonzero() {
    let tmp = TempDir::new().expect("create tempdir");
    empty_fleet_toml(tmp.path());

    let json = binstale_json_unknown("ghost-daemon2", 99997);
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
        !output.status.success(),
        "rollout plan should exit non-zero for unknown daemon"
    );
}
