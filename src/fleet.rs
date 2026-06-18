//! `fleet.rs` — load and validate `fleet.toml`.
//!
//! The fleet config is at `~/.config/rollout/fleet.toml` by default.
//! Each daemon entry specifies how to build, install, launch, and healthcheck it.
//!
//! ## Schema
//!
//! ```toml
//! [[daemon]]
//! name = "agorabus"
//! repo = "/home/user/wintermute/agorabus"        # optional
//! build_cmd = "cargo build --release"            # default
//! install_cmd = "cargo install --path . --root ~/.local"
//! launch_cmd = "agorabus serve &"
//! healthcheck = "agorabus peers | jq '.[] | .name' | grep -q agorabus"  # default
//! grace_period_secs = 5                          # default
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::RolloutError;

/// Configuration for a single daemon.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DaemonRecipe {
    /// Daemon name; must match the `comm` field from binstale output.
    pub name: String,
    /// Optional path to the source repository.
    pub repo: Option<PathBuf>,
    /// Command to build the daemon. Default: `cargo build --release`.
    #[serde(default = "default_build_cmd")]
    pub build_cmd: String,
    /// Command to install the daemon binary into `$PATH`.
    pub install_cmd: String,
    /// Command to launch the daemon (run in the repo dir if repo is set).
    pub launch_cmd: String,
    /// Optional systemd-user unit name that owns this daemon (e.g. `wm-audio.service`).
    /// Populated by `rollout fleet-gen`; consumed by `restart_daemon` to choose
    /// the `systemctl --user restart` path over the legacy SIGTERM+launch_cmd path.
    #[serde(default)]
    pub unit: Option<String>,
    /// Shell command to check if daemon re-registered successfully.
    /// Default: `agorabus peers | jq -e '.[] | .name' | grep -q <name>`
    pub healthcheck: Option<String>,
    /// Grace period (seconds) before SIGKILL if process ignores SIGTERM.
    #[serde(default = "default_grace_period")]
    pub grace_period_secs: u64,
    /// Opt-in to warm-swap restart strategy.
    ///
    /// When `true`, rollout uses the warm-swap sequence (start successor → wait
    /// for claim acquisition → stop predecessor → verify exactly one holder)
    /// instead of the hard kill-then-restart.  Overridden to `true` automatically
    /// when `claim_key` is also set.  Default: `false` (hard restart, backward-
    /// compatible).
    #[serde(default)]
    pub warm_swap: bool,
    /// agorabus claim path used by this daemon (e.g. `agorabus://daemon/wm-stt`).
    ///
    /// When set, warm-swap is unconditionally enabled for this daemon.
    /// When absent but `warm_swap = true`, the path is derived automatically as
    /// `agorabus://daemon/<unit-stem>` (where `unit-stem` is the unit filename
    /// with `.service` stripped).
    #[serde(default)]
    pub claim_key: Option<String>,
}

/// Default build command: route through cloudbuild (Hetzner) by default.
///
/// The command is resolved at config-load time from (highest priority first):
///
/// 1. `ROLLOUT_BUILD_CMD` env var — test/override injection.
/// 2. `headway` binary in `$PATH` — `headway build .` (structured verdict).
/// 3. Direct `cloudbuild.sh` path — `bash ~/.claude/skills/cloudbuild/cloudbuild.sh build .`
///
/// Never returns `cargo build --release` as a default; that is only used when
/// `--local-build` is explicitly passed at the CLI (escape hatch).
fn default_build_cmd() -> String {
    // 1. Env var override (test injection / manual override).
    if let Ok(cmd) = std::env::var("ROLLOUT_BUILD_CMD") {
        if !cmd.is_empty() {
            return cmd;
        }
    }
    // 2. headway binary present → prefer it.
    if which_headway() {
        return "headway build .".to_owned();
    }
    // 3. Fallback: direct cloudbuild.sh.
    "bash ~/.claude/skills/cloudbuild/cloudbuild.sh build .".to_owned()
}

/// Check whether `headway` is available in `$PATH`.
fn which_headway() -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v headway >/dev/null 2>&1")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

const fn default_grace_period() -> u64 {
    5
}

impl DaemonRecipe {
    /// Return the healthcheck command, using the default agorabus peers check if not set.
    #[must_use]
    pub(crate) fn healthcheck_cmd(&self) -> String {
        self.healthcheck.clone().unwrap_or_else(|| {
            format!(
                "agorabus peers | jq -e '[.[].name] | map(select(. == \"{}\")) | length > 0'",
                self.name
            )
        })
    }
}

/// Parsed fleet configuration.
#[derive(Debug, Clone, Deserialize)]
struct RawFleetConfig {
    #[serde(rename = "daemon", default)]
    daemons: Vec<DaemonRecipe>,
}

/// Loaded and indexed fleet configuration.
#[derive(Debug, Clone)]
pub(crate) struct FleetConfig {
    /// Daemon recipes indexed by name.
    recipes: HashMap<String, DaemonRecipe>,
    /// Source path for error messages.
    #[allow(dead_code)]
    source_path: PathBuf,
}

impl FleetConfig {
    /// Load fleet.toml from the given path.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or parsed.
    pub(crate) fn load(path: &Path) -> Result<Self, RolloutError> {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            RolloutError::FleetConfig(format!("cannot read {}: {e}", path.display()))
        })?;
        let raw: RawFleetConfig = toml::from_str(&contents)
            .map_err(|e| RolloutError::FleetConfig(format!("parse error in {}: {e}", path.display())))?;
        let recipes = raw
            .daemons
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect();
        Ok(Self {
            recipes,
            source_path: path.to_owned(),
        })
    }

    /// Look up a recipe by daemon name.
    #[must_use]
    pub(crate) fn get(&self, name: &str) -> Option<&DaemonRecipe> {
        self.recipes.get(name)
    }

    /// Return all daemon names in the fleet config (order unspecified).
    #[must_use]
    pub(crate) fn all_names(&self) -> Vec<String> {
        self.recipes.keys().cloned().collect()
    }

    /// Validate that all named daemons have recipes.
    ///
    /// Returns `Ok(())` if all names are covered, or
    /// `Err(RolloutError::UnknownDaemons)` listing the missing ones.
    ///
    /// # Errors
    ///
    /// Returns an error if any daemon name is not in the fleet config.
    pub(crate) fn validate_names<'a>(
        &self,
        names: impl Iterator<Item = &'a str>,
    ) -> Result<(), RolloutError> {
        let missing: Vec<String> = names
            .filter(|n| !self.recipes.contains_key(*n))
            .map(str::to_owned)
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(RolloutError::UnknownDaemons { names: missing })
        }
    }
}

/// Return the default fleet.toml path: `~/.config/rollout/fleet.toml`.
///
/// # Errors
///
/// Returns an error if the home directory cannot be determined.
pub(crate) fn default_fleet_path() -> Result<PathBuf, RolloutError> {
    let home = std::env::var("HOME")
        .map_err(|_| RolloutError::FleetConfig("$HOME not set".to_owned()))?;
    Ok(PathBuf::from(home).join(".config/rollout/fleet.toml"))
}
