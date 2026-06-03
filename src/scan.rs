//! `scan.rs` — invoke or parse `binstale scan --format json` output.
//!
//! The binstale JSON format is a JSON array of `ProcessVerdict` objects.
//! We only operate on entries where `verdict != "fresh"`.

use std::io::{self, Read};

use serde::Deserialize;

use crate::error::RolloutError;

/// A single process verdict from `binstale scan --format json`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BinstaleEntry {
    /// Process ID.
    pub pid: u32,
    /// Process comm name.
    pub comm: String,
    /// Resolved exe path (may end in ` (deleted)`).
    pub exe_path: String,
    /// Staleness verdict: "fresh", "deleted-exe", "inode-drift", "prov-stale".
    pub verdict: String,
}

impl BinstaleEntry {
    /// Returns `true` if this entry is stale (i.e., should be acted on).
    #[must_use]
    pub(crate) fn is_stale(&self) -> bool {
        self.verdict != "fresh"
    }
}

/// Source of binstale input data.
#[derive(Debug, Clone)]
pub(crate) enum ScanSource {
    /// Read from stdin (`--from -`).
    Stdin,
    /// Shell out to `binstale scan --format json` with the given match regex.
    Binstale { match_regex: Option<String> },
}

/// Collect stale entries from the specified source.
///
/// # Errors
///
/// Returns an error if binstale cannot be invoked, if stdin read fails, or if
/// the JSON is malformed.
pub(crate) fn collect_stale(source: &ScanSource) -> Result<Vec<BinstaleEntry>, RolloutError> {
    let json = match source {
        ScanSource::Stdin => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| RolloutError::BinstaleScanFailed(e.to_string()))?;
            buf
        }
        ScanSource::Binstale { match_regex } => {
            let mut cmd = std::process::Command::new("binstale");
            cmd.arg("scan").arg("--format").arg("json");
            if let Some(re) = match_regex {
                cmd.arg("--match").arg(re);
            }
            let out = cmd
                .output()
                .map_err(|e| RolloutError::BinstaleScanFailed(format!("binstale not found: {e}")))?;
            if !out.status.success() && out.stdout.is_empty() {
                return Err(RolloutError::BinstaleScanFailed(
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                ));
            }
            String::from_utf8_lossy(&out.stdout).into_owned()
        }
    };

    let all: Vec<BinstaleEntry> =
        serde_json::from_str(&json).map_err(RolloutError::Json)?;

    Ok(all.into_iter().filter(BinstaleEntry::is_stale).collect())
}

/// Parse a binstale JSON string (already read) and return only stale entries.
///
/// # Errors
///
/// Returns a JSON parse error if the string is not a valid binstale array.
pub(crate) fn parse_stale_json(json: &str) -> Result<Vec<BinstaleEntry>, RolloutError> {
    let all: Vec<BinstaleEntry> =
        serde_json::from_str(json).map_err(RolloutError::Json)?;
    Ok(all.into_iter().filter(BinstaleEntry::is_stale).collect())
}
