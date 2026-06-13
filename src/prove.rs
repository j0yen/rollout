//! `prove.rs` — `rollout prove` subcommand: run `changeover probe` and seed the proof ledger.
//!
//! `rollout prove --daemon <unit>` invokes `changeover probe <unit> --json`,
//! ingests the output through the existing record-proof path, and writes/updates
//! the per-daemon entry in `~/.config/rollout/proofs.json`.
//!
//! `--all` proves every daemon in the fleet recipe.
//! `--dry-run` prints what *would* be written without touching the ledger.

use std::path::PathBuf;
use std::process::Command;

use crate::autogate::{self, GateVerdict, ProofEntry, ProofLedger};
use crate::error::RolloutError;
use crate::fleet::{self, FleetConfig};

/// Arguments for `rollout prove`.
#[derive(Debug, clap::Args)]
pub(crate) struct ProveArgs {
    /// Prove a single daemon unit by name (e.g. `wm-audio`).
    ///
    /// Mutually exclusive with `--all`.
    #[arg(long, value_name = "UNIT", conflicts_with = "all")]
    pub daemon: Option<String>,

    /// Prove all daemons listed in fleet.toml.
    ///
    /// Mutually exclusive with `--daemon`.
    #[arg(long, conflicts_with = "daemon")]
    pub all: bool,

    /// Path to fleet.toml. Default: `~/.config/rollout/fleet.toml`.
    #[arg(long, value_name = "PATH")]
    pub fleet: Option<PathBuf>,

    /// Path to the proof ledger. Default: `~/.config/rollout/proofs.json`.
    #[arg(long, value_name = "PATH")]
    pub ledger: Option<PathBuf>,

    /// Run the probe and print the verdict without writing the ledger.
    #[arg(long)]
    pub dry_run: bool,

    /// Path to the `changeover` binary. Default: `changeover` (resolved via PATH).
    #[arg(long, value_name = "PATH", default_value = "changeover")]
    pub changeover_bin: String,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the `prove` subcommand.
///
/// # Errors
///
/// Returns an error if the fleet config cannot be loaded, `changeover probe`
/// fails to produce parseable JSON, or the ledger cannot be written.
pub(crate) fn run_prove(args: &ProveArgs) -> Result<(), RolloutError> {
    let fleet = load_fleet(args.fleet.as_deref())?;

    let daemon_names: Vec<String> = if args.all {
        fleet.all_names()
    } else if let Some(ref name) = args.daemon {
        vec![name.clone()]
    } else {
        return Err(RolloutError::FleetConfig(
            "rollout prove: specify --daemon <unit> or --all".to_owned(),
        ));
    };

    if daemon_names.is_empty() {
        println!("rollout prove: no daemons in fleet recipe. Nothing to prove.");
        return Ok(());
    }

    let ledger_path = match &args.ledger {
        Some(p) => p.clone(),
        None => autogate::default_proofs_path()?,
    };

    prove_daemons(&daemon_names, &ledger_path, args.dry_run, &args.changeover_bin)
}

// ---------------------------------------------------------------------------
// Core prove loop (shared with tests)
// ---------------------------------------------------------------------------

/// Prove the given daemons and optionally write the ledger.
///
/// # Errors
///
/// Returns an error if any probe fails to produce parseable JSON, or if any
/// daemon returns a Refuse verdict.
pub(crate) fn prove_daemons(
    daemon_names: &[String],
    ledger_path: &std::path::Path,
    dry_run: bool,
    changeover_bin: &str,
) -> Result<(), RolloutError> {
    // Load once; accumulate; save once at the end (or not at all for --dry-run).
    let mut ledger = ProofLedger::load(ledger_path)?;
    let mut any_refuse = false;
    let total = daemon_names.len();

    for (i, name) in daemon_names.iter().enumerate() {
        print!("rollout prove [{}/{}]: probing {}… ", i + 1, total, name);

        let probe_json = run_probe(changeover_bin, name)?;

        let entry: ProofEntry = serde_json::from_str(&probe_json).map_err(|e| {
            RolloutError::FleetConfig(format!(
                "changeover probe for `{name}` returned unparseable JSON: {e}\nraw output:\n{probe_json}"
            ))
        })?;

        // Compute gate verdict against the *new* proof (events-lost check).
        let verdict = compute_verdict(&entry);

        println!(
            "{}  hash={:.12}  window={}ms  events_lost={}",
            format_verdict(&verdict),
            entry.binary_hash,
            entry.deafness_ms,
            entry.events_missed_window,
        );

        if matches!(verdict, GateVerdict::Refuse { .. }) {
            any_refuse = true;
        }

        if dry_run {
            println!(
                "  [dry-run] would record proof for `{name}` → {}",
                ledger_path.display()
            );
        } else {
            ledger.upsert(entry);
        }
    }

    if !dry_run {
        ledger.save(ledger_path)?;
        println!(
            "rollout prove: ledger written → {}",
            ledger_path.display()
        );
    }

    if any_refuse {
        Err(RolloutError::ProveRefused {
            reason: "one or more daemons returned a Refuse proof".to_owned(),
        })
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Invoke `<changeover_bin> probe <daemon> --json` and return stdout.
///
/// # Errors
///
/// Returns an error if the binary cannot be spawned or exits non-zero.
fn run_probe(changeover_bin: &str, daemon: &str) -> Result<String, RolloutError> {
    let output = Command::new(changeover_bin)
        .args(["probe", daemon, "--json"])
        .output()
        .map_err(|e| RolloutError::BuildFailed {
            name: daemon.to_owned(),
            reason: format!("failed to spawn `{changeover_bin} probe`: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RolloutError::BuildFailed {
            name: daemon.to_owned(),
            reason: format!(
                "`{changeover_bin} probe {daemon}` exited {:?}: {stderr}",
                output.status.code()
            ),
        });
    }

    String::from_utf8(output.stdout).map_err(|e| {
        RolloutError::BinstaleScanFailed(format!(
            "`{changeover_bin} probe {daemon}` produced non-UTF-8 output: {e}"
        ))
    })
}

/// Evaluate whether a freshly-acquired proof allows auto-apply.
///
/// Uses the same thresholds as the live gate (default config).
fn compute_verdict(entry: &ProofEntry) -> GateVerdict {
    // Build a single-entry ledger and call the existing gate function.
    let mut ledger = ProofLedger::default();
    let hash = entry.binary_hash.clone();
    ledger.upsert(entry.clone());
    autogate::gate(&entry.daemon, &hash, &ledger, &autogate::GateConfig::default())
}

fn format_verdict(v: &GateVerdict) -> &'static str {
    match v {
        GateVerdict::Allow => "Allow",
        GateVerdict::Refuse { .. } => "Refuse",
    }
}

/// Load fleet config from the given path or the default.
fn load_fleet(path: Option<&std::path::Path>) -> Result<FleetConfig, RolloutError> {
    let p = match path {
        Some(p) => p.to_owned(),
        None => fleet::default_fleet_path()?,
    };
    FleetConfig::load(&p)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// Build a minimal fixture `ProofEntry` JSON.
    fn fixture_probe_json(
        daemon: &str,
        binary_hash: &str,
        deafness_ms: u64,
        events_missed_window: u64,
    ) -> String {
        format!(
            r#"{{"daemon":"{daemon}","binary_hash":"{binary_hash}","deafness_ms":{deafness_ms},"events_missed_window":{events_missed_window},"recorded_at":"2026-06-13T00:00:00Z"}}"#
        )
    }

    /// Create a fake `changeover` shell script that always echoes `output` and
    /// return its (owning TempDir, absolute path).
    fn fake_changeover_script(output: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin_path = dir.path().join("changeover");
        let content = format!("#!/bin/sh\necho '{}'\n", output.replace('\'', "'\\''"));
        std::fs::write(&bin_path, content).expect("write fake changeover");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let bin_str = bin_path.to_string_lossy().into_owned();
        (dir, bin_str)
    }

    /// Create a temporary path that does NOT exist yet (different from `NamedTempFile`,
    /// which creates a zero-byte file that breaks JSON parsing).
    fn nonexistent_ledger_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("proofs.json");
        (dir, path)
    }

    // AC1 / AC3: parse probe JSON and write ledger entry.
    #[test]
    fn test_prove_writes_ledger() {
        let json = fixture_probe_json("wm-audio", "abc123", 10, 0);
        let (_dir, changeover_bin) = fake_changeover_script(&json);
        let (_ldir, ledger_path) = nonexistent_ledger_path();

        let result = prove_daemons(
            &[String::from("wm-audio")],
            &ledger_path,
            false,
            &changeover_bin,
        );
        assert!(result.is_ok(), "prove should succeed: {result:?}");

        let written = std::fs::read_to_string(&ledger_path).expect("ledger written");
        assert!(written.contains("wm-audio"), "ledger must contain daemon name");
        assert!(written.contains("abc123"), "ledger must contain binary hash");
    }

    // AC2: exit non-zero on Refuse proof (`events_missed_window` > 0).
    #[test]
    fn test_prove_refuse_exits_nonzero() {
        let json = fixture_probe_json("wm-audio", "abc123", 10, 5);
        let (_dir, changeover_bin) = fake_changeover_script(&json);
        let (_ldir, ledger_path) = nonexistent_ledger_path();

        let result = prove_daemons(
            &[String::from("wm-audio")],
            &ledger_path,
            false,
            &changeover_bin,
        );
        assert!(result.is_err(), "should be Err on Refuse verdict");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Refuse"),
            "error should mention Refuse: {err_msg}"
        );
    }

    // AC3 (`--dry-run`): does not write ledger.
    #[test]
    fn test_prove_dry_run_no_write() {
        let json = fixture_probe_json("wm-audio", "abc123", 10, 0);
        let (_dir, changeover_bin) = fake_changeover_script(&json);
        let (_ldir, ledger_path) = nonexistent_ledger_path();

        let result = prove_daemons(
            &[String::from("wm-audio")],
            &ledger_path,
            true, // dry_run
            &changeover_bin,
        );
        assert!(result.is_ok(), "dry-run should succeed: {result:?}");

        // Ledger file must not have been created.
        assert!(
            !ledger_path.exists(),
            "dry-run must not create ledger file"
        );
    }

    // AC4: changing `binary_hash` in the recorded proof invalidates the old entry.
    #[test]
    fn test_hash_change_invalidates_prior_proof() {
        let json1 = fixture_probe_json("wm-audio", "hash1", 0, 0);
        let (_dir1, bin1) = fake_changeover_script(&json1);
        let (_ldir, ledger_path) = nonexistent_ledger_path();

        prove_daemons(&[String::from("wm-audio")], &ledger_path, false, &bin1)
            .expect("first prove");

        let ledger = ProofLedger::load(&ledger_path).expect("load");
        assert_eq!(ledger.get("wm-audio").expect("entry").binary_hash, "hash1");

        // Prove again with a different hash; ledger must be updated.
        let json2 = fixture_probe_json("wm-audio", "hash2", 0, 0);
        let (_dir2, bin2) = fake_changeover_script(&json2);
        prove_daemons(&[String::from("wm-audio")], &ledger_path, false, &bin2)
            .expect("second prove");

        let ledger2 = ProofLedger::load(&ledger_path).expect("load2");
        assert_eq!(
            ledger2.get("wm-audio").expect("entry2").binary_hash,
            "hash2",
            "ledger must reflect updated hash"
        );

        // A gate check against the OLD hash must Refuse.
        let verdict = autogate::gate(
            "wm-audio",
            "hash1",
            &ledger2,
            &autogate::GateConfig::default(),
        );
        assert!(
            matches!(verdict, autogate::GateVerdict::Refuse { .. }),
            "old hash must produce Refuse after hash changes"
        );
    }

    // AC6 (fixture-based): `proofs.json` has one entry per fleet daemon.
    #[test]
    fn test_prove_all_produces_one_entry_per_daemon() {
        let (_ldir, ledger_path) = nonexistent_ledger_path();

        let json_audio = fixture_probe_json("wm-audio", "h1", 0, 0);
        let (_dir1, bin_audio) = fake_changeover_script(&json_audio);
        prove_daemons(&[String::from("wm-audio")], &ledger_path, false, &bin_audio)
            .expect("audio");

        let json_stt = fixture_probe_json("wm-stt", "h2", 0, 0);
        let (_dir2, bin_stt) = fake_changeover_script(&json_stt);
        prove_daemons(&[String::from("wm-stt")], &ledger_path, false, &bin_stt)
            .expect("stt");

        let ledger = ProofLedger::load(&ledger_path).expect("load");
        assert!(ledger.get("wm-audio").is_some(), "wm-audio entry must exist");
        assert!(ledger.get("wm-stt").is_some(), "wm-stt entry must exist");
    }
}

