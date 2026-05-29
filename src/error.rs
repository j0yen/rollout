//! Error types for rollout.

use thiserror::Error;

/// Top-level error type for the rollout CLI.
#[derive(Debug, Error)]
pub(crate) enum RolloutError {
    /// One or more daemons listed in the binstale JSON have no entry in fleet.toml.
    #[error("unknown daemons (not in fleet.toml): {}", names.join(", "))]
    UnknownDaemons { names: Vec<String> },

    /// Could not read or parse the binstale JSON input.
    #[error("binstale scan failed: {0}")]
    BinstaleScanFailed(String),

    /// Could not read or parse fleet.toml.
    #[error("fleet.toml error: {0}")]
    FleetConfig(String),

    /// A daemon failed to re-register within the healthcheck timeout.
    #[error("daemon `{name}` failed healthcheck (old_pid={old_pid}, new_pid={new_pid:?}): {reason}")]
    HealthcheckTimeout {
        name: String,
        old_pid: u32,
        new_pid: Option<u32>,
        reason: String,
    },

    /// A daemon's build or install command failed.
    #[error("build/install step for `{name}` failed: {reason}")]
    BuildFailed { name: String, reason: String },

    /// A daemon could not be launched.
    #[error("launch of `{name}` failed: {reason}")]
    LaunchFailed { name: String, reason: String },

    /// The --window guard detected recent voice activity and refused to restart.
    #[error("window guard: daemon `{name}` has recent wm.dialog.turn.* activity; use --window to change the quiet period or wait")]
    WindowGuardBlocked { name: String },

    /// I/O or subprocess error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Signal delivery error.
    #[error("signal error: {0}")]
    Signal(String),

    /// Binstale JSON deserialization error.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}
