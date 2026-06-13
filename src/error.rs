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

    /// One or more voice daemons were deferred because a turn or session was
    /// in flight (or the bus was unreachable).  The daemons were NOT restarted.
    #[error(
        "voice-active deferral: {count} daemon(s) were not restarted ({names}); \
         retry once the voice turn is complete"
    )]
    VoiceActivityDeferred { names: String, count: usize },

    /// I/O or subprocess error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Signal delivery error.
    #[error("signal error: {0}")]
    Signal(String),

    /// Binstale JSON deserialization error.
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    /// One or more daemons produced a Refuse verdict during `rollout prove`.
    #[error("prove Refuse: {reason}")]
    ProveRefused { reason: String },
}
