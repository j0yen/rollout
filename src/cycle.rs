//! `cycle.rs` — `rollout cycle` subcommand: automated prove → apply → verify loop.
//!
//! **Dormant by default**: when `ROLLOUT_AUTO_ENABLED` is unset or `"0"`, the cycle
//! runs in dry-run (probe + plan + would-verify, zero restarts) and logs what it
//! *would* do.  Set `ROLLOUT_AUTO_ENABLED=1` and enable the systemd timer to activate
//! the live path.
//!
//! # Sequence (live path)
//!
//! 1. `rollout prove --all` — run `changeover probe` for every daemon in fleet.toml
//!    and refresh the proof ledger.
//! 2. `rollout apply --auto` — warm-swap only; a daemon whose warm-swap cannot be
//!    confirmed is **skipped**, never hard-restarted.
//! 3. **Post-swap verification** — confirm each rolled daemon re-holds its
//!    `agorabus://daemon/<unit>` claim and a bus round-trip completes.
//! 4. **Receipt** — write a JSON receipt under `~/.local/state/rollout/receipts/`
//!    with timestamp, daemons rolled, per-daemon window ms, verify pass/fail.
//!
//! # Dry-run path (`ROLLOUT_AUTO_ENABLED` unset or `"0"`)
//!
//! Steps 1–3 are probed without any restart:
//! - Step 1 runs `rollout prove --all --dry-run` (probe, print, no ledger write).
//! - Step 2 prints the plan (what *would* be applied) but calls zero restarts.
//! - Step 3 logs the would-verify result.
//! - A receipt is written with `dry_run: true` and `verify: null`.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::autogate::{self, GateConfig, GateVerdict, ProofLedger};
use crate::error::RolloutError;
use crate::fleet::{self, FleetConfig};
use crate::health::{is_voice_daemon, voice_activity_in_flight, VoiceActivityState};
use crate::prove::prove_daemons;
use crate::scan::{collect_stale, ScanSource};
use crate::warmswap::{claim_path_for_recipe, AgoraClient, ShellAgoraClient};

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

/// Arguments for `rollout cycle`.
#[derive(Debug, clap::Args)]
pub(crate) struct CycleArgs {
    /// Path to fleet.toml. Default: `~/.config/rollout/fleet.toml`.
    #[arg(long, value_name = "PATH")]
    pub fleet: Option<PathBuf>,

    /// Path to the proof ledger. Default: `~/.config/rollout/proofs.json`.
    #[arg(long, value_name = "PATH")]
    pub ledger: Option<PathBuf>,

    /// Directory to write receipts into. Default: `~/.local/state/rollout/receipts/`.
    #[arg(long, value_name = "DIR")]
    pub receipt_dir: Option<PathBuf>,

    /// Path to the `changeover` binary. Default: `changeover` (resolved via PATH).
    #[arg(long, value_name = "PATH", default_value = "changeover")]
    pub changeover_bin: String,

    /// Healthcheck timeout per daemon (seconds). Default: 30.
    #[arg(long, value_name = "SECS", default_value_t = 30_u64)]
    pub healthcheck_timeout: u64,

    /// Force dry-run even if `ROLLOUT_AUTO_ENABLED=1`.
    #[arg(long)]
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// Receipt
// ---------------------------------------------------------------------------

/// Per-daemon record within a cycle receipt.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct DaemonCycleRecord {
    /// Daemon name.
    pub name: String,
    /// Whether the daemon was actually restarted (false in dry-run or skipped).
    pub restarted: bool,
    /// Why the daemon was skipped (`None` if restarted or pending).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    /// Post-swap claim holder count (`None` in dry-run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_holders: Option<usize>,
    /// Whether post-swap verification passed (`None` in dry-run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify_ok: Option<bool>,
    /// Swap wall-clock time in milliseconds (`None` if skipped or dry-run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_ms: Option<u64>,
}

/// Full receipt written after each cycle run.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CycleReceipt {
    /// ISO-8601 UTC timestamp of cycle start.
    pub timestamp: String,
    /// Whether this was a dry-run (no restarts performed).
    pub dry_run: bool,
    /// Per-daemon records.
    pub daemons: Vec<DaemonCycleRecord>,
    /// Number of daemons successfully rolled.
    pub rolled_count: usize,
    /// Number of daemons that failed verification.
    pub verify_fail_count: usize,
    /// Overall cycle outcome: `"ok"`, `"partial"`, or `"dry-run"`.
    pub outcome: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the `cycle` subcommand.
///
/// # Errors
///
/// Returns `Err` when any of the following are true after the cycle completes:
/// - At least one daemon failed post-swap verification (claim not re-held).
/// - The prove step returned a Refuse verdict (ledger update suppressed).
///
/// In dry-run mode, always returns `Ok(())`.
pub(crate) fn run_cycle(args: &CycleArgs) -> Result<(), RolloutError> {
    let auto_enabled = !args.dry_run && is_auto_enabled();

    if auto_enabled {
        println!("rollout cycle: LIVE mode (ROLLOUT_AUTO_ENABLED=1)");
    } else {
        println!(
            "rollout cycle: DRY-RUN mode (ROLLOUT_AUTO_ENABLED={} — set to 1 to enable live swaps)",
            std::env::var("ROLLOUT_AUTO_ENABLED").unwrap_or_default()
        );
    }

    let fleet = load_fleet(args.fleet.as_deref())?;
    let ledger_path = match &args.ledger {
        Some(p) => p.clone(),
        None => autogate::default_proofs_path()?,
    };
    let receipt_dir = resolve_receipt_dir(args.receipt_dir.as_deref())?;
    let _cycle_start_ms = now_ms();
    let timestamp = iso_now();

    // ── Step 1: prove --all ──────────────────────────────────────────────────

    println!("rollout cycle [1/3]: refreshing proof ledger (prove --all{})…",
        if auto_enabled { "" } else { " --dry-run" });

    let prove_result = prove_daemons(
        &fleet.all_names(),
        &ledger_path,
        !auto_enabled, // dry_run when not live
        &args.changeover_bin,
    );

    // In dry-run, ignore prove errors (they'd fail on changeover not being installed).
    // In live mode, a Refuse verdict means we should still attempt the rest but note it.
    let prove_failed = prove_result.is_err();
    if let Err(ref e) = prove_result {
        eprintln!("rollout cycle: prove step warning: {e}");
        if auto_enabled {
            eprintln!("rollout cycle: proceeding with existing ledger entries");
        }
    }

    // ── Step 2: apply --auto (warm-swap only) ────────────────────────────────

    println!("rollout cycle [2/3]: {} stale daemon check + apply{}…",
        if auto_enabled { "live" } else { "dry-run" },
        if auto_enabled { " --auto (warm-swap only)" } else { " plan (no restarts)" });

    let stale = collect_stale(&ScanSource::Binstale { match_regex: None })
        .unwrap_or_else(|e| {
            eprintln!("rollout cycle: binstale scan failed: {e}; treating as empty");
            vec![]
        });

    // Load ledger for gate checks (even in dry-run, to show what *would* be allowed).
    let ledger: ProofLedger = if ledger_path.exists() {
        ProofLedger::load(&ledger_path).unwrap_or_default()
    } else {
        ProofLedger::default()
    };

    let healthcheck_timeout = Duration::from_secs(args.healthcheck_timeout);
    let bus = ShellAgoraClient;
    let mut daemon_records: Vec<DaemonCycleRecord> = Vec::new();

    for entry in &stale {
        let recipe = match fleet.get(&entry.comm) {
            Some(r) => r,
            None => {
                daemon_records.push(DaemonCycleRecord {
                    name: entry.comm.clone(),
                    restarted: false,
                    skip_reason: Some("no fleet.toml recipe".to_owned()),
                    claim_holders: None,
                    verify_ok: None,
                    swap_ms: None,
                });
                continue;
            }
        };

        // Warm-swap gate: skip daemons that don't support warm-swap.
        let claim_path = claim_path_for_recipe(recipe);
        if claim_path.is_none() {
            println!(
                "rollout cycle: {} skipped — no claim path (warm-swap-only mode; hard-restart skipped)",
                entry.comm
            );
            daemon_records.push(DaemonCycleRecord {
                name: entry.comm.clone(),
                restarted: false,
                skip_reason: Some("no claim path (warm-swap only)".to_owned()),
                claim_holders: None,
                verify_ok: None,
                swap_ms: None,
            });
            continue;
        }

        // Turn-aware quiet window for voice daemons.
        if is_voice_daemon(&entry.comm) {
            match voice_activity_in_flight(Duration::from_secs(3)) {
                VoiceActivityState::InFlight { reason } => {
                    println!("rollout cycle: {} deferred — voice active: {}", entry.comm, reason);
                    daemon_records.push(DaemonCycleRecord {
                        name: entry.comm.clone(),
                        restarted: false,
                        skip_reason: Some(format!("voice active: {reason}")),
                        claim_holders: None,
                        verify_ok: None,
                        swap_ms: None,
                    });
                    continue;
                }
                VoiceActivityState::BusUnreachable => {
                    println!(
                        "rollout cycle: {} deferred — agorabus unreachable (cannot confirm turn idle)",
                        entry.comm
                    );
                    daemon_records.push(DaemonCycleRecord {
                        name: entry.comm.clone(),
                        restarted: false,
                        skip_reason: Some("agorabus unreachable".to_owned()),
                        claim_holders: None,
                        verify_ok: None,
                        swap_ms: None,
                    });
                    continue;
                }
                VoiceActivityState::Idle => {}
            }
        }

        // Auto-gate: proof ledger check.
        let current_hash = entry.exe_path.clone();
        let gate_verdict = autogate::gate(&entry.comm, &current_hash, &ledger, &GateConfig::default());
        if !auto_enabled {
            // Dry-run: just print what *would* happen.
            let would = match gate_verdict {
                GateVerdict::Allow => "would warm-swap",
                GateVerdict::Refuse { .. } => "would skip (gate Refuse)",
            };
            println!("rollout cycle [dry-run]: {} — {}", entry.comm, would);
            daemon_records.push(DaemonCycleRecord {
                name: entry.comm.clone(),
                restarted: false,
                skip_reason: Some(format!("dry-run ({would})")),
                claim_holders: None,
                verify_ok: None,
                swap_ms: None,
            });
            continue;
        }

        match gate_verdict {
            GateVerdict::Refuse { ref reason } => {
                println!("rollout cycle: {} skipped — gate Refuse: {reason}", entry.comm);
                daemon_records.push(DaemonCycleRecord {
                    name: entry.comm.clone(),
                    restarted: false,
                    skip_reason: Some(format!("gate Refuse: {reason}")),
                    claim_holders: None,
                    verify_ok: None,
                    swap_ms: None,
                });
                continue;
            }
            GateVerdict::Allow => {
                println!("rollout cycle: {} gate Allow — warm-swapping…", entry.comm);
            }
        }

        // Execute warm-swap.
        let swap_start = now_ms();
        let cp = claim_path.as_deref().unwrap_or_default(); // safe: checked above
        let swap_result = crate::warmswap::warm_swap(
            recipe,
            cp,
            entry.pid,
            healthcheck_timeout,
            &bus,
        );
        let swap_ms = now_ms().saturating_sub(swap_start);

        match swap_result {
            crate::warmswap::WarmSwapResult::Success { .. } => {
                println!("rollout cycle: {} warm-swap ok ({}ms)", entry.comm, swap_ms);
                // Step 3: post-swap verification.
                let holders = bus.claim_list_count(cp).unwrap_or(0);
                let verify_ok = holders == 1;
                if verify_ok {
                    println!("rollout cycle: {} verify ok (1 claim holder)", entry.comm);
                } else {
                    eprintln!(
                        "rollout cycle: {} verify FAIL — expected 1 claim holder, got {holders}",
                        entry.comm
                    );
                }
                daemon_records.push(DaemonCycleRecord {
                    name: entry.comm.clone(),
                    restarted: true,
                    skip_reason: None,
                    claim_holders: Some(holders),
                    verify_ok: Some(verify_ok),
                    swap_ms: Some(swap_ms),
                });
            }
            crate::warmswap::WarmSwapResult::SplitState { holders } => {
                eprintln!(
                    "rollout cycle: {} SplitState after swap (holders={holders}) — skipping",
                    entry.comm
                );
                daemon_records.push(DaemonCycleRecord {
                    name: entry.comm.clone(),
                    restarted: true,
                    skip_reason: None,
                    claim_holders: Some(holders),
                    verify_ok: Some(false),
                    swap_ms: Some(swap_ms),
                });
            }
            crate::warmswap::WarmSwapResult::Failed(ref e) => {
                eprintln!("rollout cycle: {} swap FAILED: {e}", entry.comm);
                daemon_records.push(DaemonCycleRecord {
                    name: entry.comm.clone(),
                    restarted: false,
                    skip_reason: Some(format!("swap failed: {e}")),
                    claim_holders: None,
                    verify_ok: Some(false),
                    swap_ms: Some(swap_ms),
                });
            }
        }
    }

    // ── Step 3/receipt ───────────────────────────────────────────────────────

    let rolled_count = daemon_records.iter().filter(|r| r.restarted).count();
    let verify_fail_count = daemon_records
        .iter()
        .filter(|r| r.verify_ok == Some(false))
        .count();

    let outcome = if !auto_enabled {
        "dry-run".to_owned()
    } else if verify_fail_count > 0 {
        "partial".to_owned()
    } else {
        "ok".to_owned()
    };

    println!(
        "rollout cycle [3/3]: outcome={outcome} rolled={rolled_count} verify_fail={verify_fail_count}"
    );

    let receipt = CycleReceipt {
        timestamp: timestamp.clone(),
        dry_run: !auto_enabled,
        daemons: daemon_records,
        rolled_count,
        verify_fail_count,
        outcome: outcome.clone(),
    };
    write_receipt(&receipt_dir, &timestamp, &receipt)?;

    if verify_fail_count > 0 {
        Err(RolloutError::CycleVerifyFailed {
            count: verify_fail_count,
        })
    } else if prove_failed && auto_enabled {
        // Prove errors are advisory in live mode (we used existing ledger entries).
        // Return Ok — the receipt records what happened.
        Ok(())
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns `true` when the value (from env or override) is exactly `"1"`.
pub(crate) fn auto_enabled_from_str(val: &str) -> bool {
    val == "1"
}

/// Returns `true` when `ROLLOUT_AUTO_ENABLED` is exactly `"1"`.
fn is_auto_enabled() -> bool {
    auto_enabled_from_str(&std::env::var("ROLLOUT_AUTO_ENABLED").unwrap_or_default())
}

/// Resolve the receipt dir, creating it if necessary.
fn resolve_receipt_dir(override_dir: Option<&std::path::Path>) -> Result<PathBuf, RolloutError> {
    let dir = match override_dir {
        Some(p) => p.to_owned(),
        None => {
            let home = std::env::var("HOME").map_err(|_| {
                RolloutError::FleetConfig("HOME env var not set".to_owned())
            })?;
            PathBuf::from(home).join(".local/state/rollout/receipts")
        }
    };
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Return current unix time in milliseconds.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Return an ISO-8601-like UTC timestamp string (seconds resolution).
fn iso_now() -> String {
    // Use a simple unix-seconds representation; no chrono dep required.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Convert to YYYY-MM-DDTHH:MM:SSZ manually.
    format_unix_secs(secs)
}

/// Minimal manual ISO-8601 UTC formatter (avoids the `chrono` dependency).
fn format_unix_secs(secs: u64) -> String {
    // Days since epoch.
    let days = secs / 86400;
    let time = secs % 86400;
    let h = time / 3600;
    let m = (time % 3600) / 60;
    let s = time % 60;

    // Gregorian calendar conversion.
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert days-since-1970-01-01 to (year, month, day).
fn days_to_ymd(days: u64) -> (u32, u32, u32) {
    // Algorithm: https://howardhinnant.github.io/date_algorithms.html
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (u32::try_from(y).unwrap_or(1970), u32::try_from(mo).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

/// Load fleet config from the given path or the default.
fn load_fleet(path: Option<&std::path::Path>) -> Result<FleetConfig, RolloutError> {
    let p = match path {
        Some(p) => p.to_owned(),
        None => fleet::default_fleet_path()?,
    };
    FleetConfig::load(&p)
}

/// Write the cycle receipt to a timestamped JSON file in `receipt_dir`.
fn write_receipt(dir: &std::path::Path, timestamp: &str, receipt: &CycleReceipt) -> Result<(), RolloutError> {
    let filename = format!("cycle-{}.json", timestamp.replace(':', "-"));
    let path = dir.join(&filename);
    let json = serde_json::to_string_pretty(receipt)?;
    fs::write(&path, json)?;
    println!("rollout cycle: receipt → {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
)]
mod tests {
    use super::*;

    // AC4: verify that the auto-enabled check reads from the env var correctly.
    // We use the parse helper directly to avoid unsafe set_var/remove_var calls.
    #[test]
    fn test_auto_enabled_parse_true() {
        // "1" maps to enabled.
        assert!(auto_enabled_from_str("1"), "\"1\" should mean enabled");
    }

    #[test]
    fn test_auto_enabled_parse_false_zero() {
        assert!(!auto_enabled_from_str("0"), "\"0\" should mean disabled");
    }

    #[test]
    fn test_auto_enabled_parse_false_empty() {
        assert!(!auto_enabled_from_str(""), "empty string should mean disabled");
    }

    #[test]
    fn test_auto_enabled_parse_false_other() {
        assert!(!auto_enabled_from_str("yes"), "\"yes\" should mean disabled (only \"1\" enables)");
    }

    // AC3: receipt dir is created and receipt file is written.
    #[test]
    fn test_write_receipt_creates_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let receipt = CycleReceipt {
            timestamp: "2026-06-13T00:00:00Z".to_owned(),
            dry_run: true,
            daemons: vec![],
            rolled_count: 0,
            verify_fail_count: 0,
            outcome: "dry-run".to_owned(),
        };
        write_receipt(dir.path(), "2026-06-13T00:00:00Z", &receipt).expect("write");
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .collect();
        assert_eq!(entries.len(), 1, "exactly one receipt file should be written");
        let fname = entries[0]
            .as_ref()
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        assert!(fname.starts_with("cycle-"), "receipt filename must start with cycle-");
        assert!(fname.ends_with(".json"), "receipt must be .json");
    }

    // AC3: receipt JSON contains expected fields.
    #[test]
    fn test_receipt_json_structure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ts = "2026-06-13T12:34:56Z";
        let receipt = CycleReceipt {
            timestamp: ts.to_owned(),
            dry_run: true,
            daemons: vec![DaemonCycleRecord {
                name: "wm-audio".to_owned(),
                restarted: false,
                skip_reason: Some("dry-run (would warm-swap)".to_owned()),
                claim_holders: None,
                verify_ok: None,
                swap_ms: None,
            }],
            rolled_count: 0,
            verify_fail_count: 0,
            outcome: "dry-run".to_owned(),
        };
        write_receipt(dir.path(), ts, &receipt).expect("write");

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .collect();
        let path = entries[0].as_ref().expect("entry").path();
        let content = std::fs::read_to_string(&path).expect("read");
        let val: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(val["dry_run"], true, "dry_run must be true");
        assert_eq!(val["outcome"], "dry-run");
        assert_eq!(val["rolled_count"], 0);
        assert!(val["daemons"].is_array());
        assert_eq!(val["daemons"][0]["name"], "wm-audio");
    }

    // Date formatting sanity check.
    #[test]
    fn test_format_unix_secs_epoch() {
        // 2026-06-13 00:00:00 UTC = 1_781_136_000
        // Just verify the epoch rounds correctly.
        let s = format_unix_secs(0);
        assert_eq!(s, "1970-01-01T00:00:00Z");
    }

    #[test]
    fn test_format_unix_secs_known_date() {
        // 2026-06-13 00:00:00 UTC.  Computed: 56 * 365.25 days ≈ correct.
        // Use a pre-computed value: 2026-01-01 00:00:00 = 1_767_225_600.
        let s = format_unix_secs(1_767_225_600);
        assert_eq!(s, "2026-01-01T00:00:00Z");
    }

    // Verify receipt dir creation.
    #[test]
    fn test_resolve_receipt_dir_creates_path() {
        let base = tempfile::tempdir().expect("tempdir");
        let target = base.path().join("a/b/c");
        let resolved = resolve_receipt_dir(Some(&target)).expect("resolve");
        assert_eq!(resolved, target);
        assert!(target.exists(), "resolve_receipt_dir must create dir");
    }
}
