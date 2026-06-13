//! `autogate.rs` — per-daemon proof ledger and auto-apply gate logic.
//!
//! The proof ledger lives at `~/.config/rollout/proofs.json` (XDG-respecting).
//! Each entry records the output of a `changeover probe` run, bound to the
//! daemon's binary hash at the time of measurement.
//!
//! The gate is purely functional: given a daemon name, its current binary hash,
//! the ledger, and threshold config, it returns `Allow` or `Refuse { reason }`.
//! No I/O, no side-effects — fully unit-testable without a live fleet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::RolloutError;

// ---------------------------------------------------------------------------
// Proof entry (written by `rollout record-proof --from <probe-json>`)
// ---------------------------------------------------------------------------

/// A single per-daemon proof entry produced by `changeover probe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProofEntry {
    /// Daemon name (matches fleet.toml `name`).
    pub daemon: String,
    /// SHA-256 hex digest of the installed binary at time of measurement.
    pub binary_hash: String,
    /// Observed deafness window during the swap, in milliseconds.
    pub deafness_ms: u64,
    /// Number of agorabus events missed during the swap window.
    pub events_missed_window: u64,
    /// ISO-8601 UTC timestamp when the proof was recorded.
    pub recorded_at: String,
    /// Optional strategy label (e.g. `"warm-swap"` or `"hard-restart"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
}

// ---------------------------------------------------------------------------
// Proof ledger
// ---------------------------------------------------------------------------

/// The proof ledger: a map of daemon name → most-recent proof entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ProofLedger {
    #[serde(flatten)]
    entries: HashMap<String, ProofEntry>,
}

impl ProofLedger {
    /// Load the ledger from `path`, or return an empty ledger if the file does
    /// not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub(crate) fn load(path: &Path) -> Result<Self, RolloutError> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let ledger: Self = serde_json::from_str(&s).map_err(|e| {
                    RolloutError::FleetConfig(format!(
                        "proofs.json parse error at {}: {e}",
                        path.display()
                    ))
                })?;
                Ok(ledger)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(RolloutError::FleetConfig(format!(
                "cannot read {}: {e}",
                path.display()
            ))),
        }
    }

    /// Persist the ledger to `path`, creating parent directories as needed.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub(crate) fn save(&self, path: &Path) -> Result<(), RolloutError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                RolloutError::FleetConfig(format!(
                    "cannot create dir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json).map_err(|e| {
            RolloutError::FleetConfig(format!("cannot write {}: {e}", path.display()))
        })
    }

    /// Insert or replace the entry for the named daemon.
    pub(crate) fn upsert(&mut self, entry: ProofEntry) {
        self.entries.insert(entry.daemon.clone(), entry);
    }

    /// Return the proof for `daemon`, if any.
    #[must_use]
    pub(crate) fn get(&self, daemon: &str) -> Option<&ProofEntry> {
        self.entries.get(daemon)
    }
}

// ---------------------------------------------------------------------------
// Gate config
// ---------------------------------------------------------------------------

/// Thresholds used by the gate when deciding whether to allow an auto-apply.
#[derive(Debug, Clone)]
pub(crate) struct GateConfig {
    /// Maximum allowed number of missed events. Default: 0.
    pub max_events_lost: u64,
    /// Optional maximum allowed deafness window (ms). `None` = no limit.
    pub max_deafness_ms: Option<u64>,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            max_events_lost: 0,
            max_deafness_ms: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Gate verdict
// ---------------------------------------------------------------------------

/// The gate's verdict for a single daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateVerdict {
    /// The daemon has a fresh, matching proof that passes all thresholds — go ahead.
    Allow,
    /// The daemon was refused; `reason` explains why.
    Refuse {
        /// Human-readable reason for the refusal.
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Gate logic (pure, no I/O)
// ---------------------------------------------------------------------------

/// Evaluate the auto-apply gate for one daemon.
///
/// Returns [`GateVerdict::Allow`] only when:
/// - A proof exists in the ledger for `daemon`,
/// - `proof.binary_hash == current_binary_hash`,
/// - `proof.events_missed_window <= config.max_events_lost`,
/// - and (if set) `proof.deafness_ms <= config.max_deafness_ms`.
///
/// Otherwise returns [`GateVerdict::Refuse`] with an explanatory reason.
#[must_use]
pub(crate) fn gate(
    daemon: &str,
    current_binary_hash: &str,
    ledger: &ProofLedger,
    config: &GateConfig,
) -> GateVerdict {
    let Some(proof) = ledger.get(daemon) else {
        return GateVerdict::Refuse {
            reason: format!("no proof found for daemon `{daemon}`; run `changeover probe` first"),
        };
    };

    if proof.binary_hash != current_binary_hash {
        return GateVerdict::Refuse {
            reason: format!(
                "stale proof: proof binary_hash={} but current binary_hash={current_binary_hash}; \
                 re-run `changeover probe` after the latest build",
                proof.binary_hash
            ),
        };
    }

    if proof.events_missed_window > config.max_events_lost {
        return GateVerdict::Refuse {
            reason: format!(
                "events_missed_window={} exceeds max_events_lost={}; swap was not loss-free",
                proof.events_missed_window, config.max_events_lost
            ),
        };
    }

    if let Some(max_deaf) = config.max_deafness_ms {
        if proof.deafness_ms > max_deaf {
            return GateVerdict::Refuse {
                reason: format!(
                    "deafness_ms={} exceeds max_deafness_ms={max_deaf}",
                    proof.deafness_ms
                ),
            };
        }
    }

    GateVerdict::Allow
}

// ---------------------------------------------------------------------------
// Default proofs.json path
// ---------------------------------------------------------------------------

/// Return the default path for the proof ledger: `~/.config/rollout/proofs.json`.
///
/// # Errors
///
/// Returns an error if `$HOME` is not set.
pub(crate) fn default_proofs_path() -> Result<PathBuf, RolloutError> {
    let home = std::env::var("HOME")
        .map_err(|_| RolloutError::FleetConfig("$HOME not set".to_owned()))?;
    Ok(PathBuf::from(home).join(".config/rollout/proofs.json"))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ledger(entries: Vec<ProofEntry>) -> ProofLedger {
        let mut ledger = ProofLedger::default();
        for e in entries {
            ledger.upsert(e);
        }
        ledger
    }

    fn make_proof(
        daemon: &str,
        binary_hash: &str,
        deafness_ms: u64,
        events_missed_window: u64,
    ) -> ProofEntry {
        ProofEntry {
            daemon: daemon.to_owned(),
            binary_hash: binary_hash.to_owned(),
            deafness_ms,
            events_missed_window,
            recorded_at: "2026-06-13T00:00:00Z".to_owned(),
            strategy: None,
        }
    }

    // AC3: gate returns Refuse when no proof exists for a daemon.
    #[test]
    fn test_gate_no_proof() {
        let ledger = ProofLedger::default();
        let config = GateConfig::default();
        let verdict = gate("wm-audio", "abc123", &ledger, &config);
        assert!(
            matches!(verdict, GateVerdict::Refuse { .. }),
            "expected Refuse when no proof exists"
        );
        if let GateVerdict::Refuse { reason } = verdict {
            assert!(reason.contains("no proof"), "reason should mention no proof: {reason}");
        }
    }

    // AC4a: gate returns Refuse{reason: stale} when binary_hash differs.
    #[test]
    fn test_gate_stale_hash() {
        let ledger = make_ledger(vec![make_proof("wm-audio", "oldhash", 0, 0)]);
        let config = GateConfig::default();
        let verdict = gate("wm-audio", "newhash", &ledger, &config);
        assert!(
            matches!(verdict, GateVerdict::Refuse { .. }),
            "expected Refuse for mismatched hash"
        );
        if let GateVerdict::Refuse { reason } = verdict {
            assert!(reason.contains("stale"), "reason should mention stale: {reason}");
        }
    }

    // AC4b: gate returns Allow when binary_hash matches and events_missed_window == 0.
    #[test]
    fn test_gate_allow_matching_hash_zero_events() {
        let ledger = make_ledger(vec![make_proof("wm-audio", "goodhash", 0, 0)]);
        let config = GateConfig::default();
        let verdict = gate("wm-audio", "goodhash", &ledger, &config);
        assert_eq!(verdict, GateVerdict::Allow, "expected Allow for matching hash + 0 events lost");
    }

    // AC5: gate returns Refuse when events_missed_window exceeds max_events_lost.
    #[test]
    fn test_gate_events_exceeded() {
        let ledger = make_ledger(vec![make_proof("wm-audio", "goodhash", 0, 3)]);
        let config = GateConfig { max_events_lost: 0, max_deafness_ms: None };
        let verdict = gate("wm-audio", "goodhash", &ledger, &config);
        assert!(
            matches!(verdict, GateVerdict::Refuse { .. }),
            "expected Refuse when events_missed_window > max_events_lost"
        );
        if let GateVerdict::Refuse { reason } = verdict {
            assert!(
                reason.contains("events_missed_window"),
                "reason should mention events_missed_window: {reason}"
            );
        }
    }

    // Additional: deafness_ms guard works when max_deafness_ms is set.
    #[test]
    fn test_gate_deafness_exceeded() {
        let ledger = make_ledger(vec![make_proof("wm-audio", "goodhash", 500, 0)]);
        let config = GateConfig { max_events_lost: 0, max_deafness_ms: Some(100) };
        let verdict = gate("wm-audio", "goodhash", &ledger, &config);
        assert!(
            matches!(verdict, GateVerdict::Refuse { .. }),
            "expected Refuse when deafness_ms > max_deafness_ms"
        );
    }

    // Additional: deafness_ms below threshold is fine.
    #[test]
    fn test_gate_deafness_within_limit() {
        let ledger = make_ledger(vec![make_proof("wm-audio", "goodhash", 50, 0)]);
        let config = GateConfig { max_events_lost: 0, max_deafness_ms: Some(100) };
        let verdict = gate("wm-audio", "goodhash", &ledger, &config);
        assert_eq!(verdict, GateVerdict::Allow);
    }

    // Ledger upsert replaces, not duplicates.
    #[test]
    fn test_ledger_upsert_replaces() {
        let mut ledger = ProofLedger::default();
        ledger.upsert(make_proof("wm-audio", "hash1", 0, 0));
        ledger.upsert(make_proof("wm-audio", "hash2", 0, 0));
        let entry = ledger.get("wm-audio").expect("entry must exist");
        assert_eq!(entry.binary_hash, "hash2", "second upsert should replace first");
    }
}
