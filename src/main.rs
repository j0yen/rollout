//! rollout — safe rolling restart for the live fleet.
//!
//! Consumes `binstale scan --format json`, loads per-daemon launch recipes from
//! `~/.config/rollout/fleet.toml`, and either prints the restart plan (`plan`)
//! or executes it serially (`apply`).
//!
//! # Safety posture
//!
//! - `plan` is the default subcommand; `apply` is the only mutating path.
//! - `apply` serializes strictly: at most one daemon is mid-restart at any time.
//! - A daemon with no recipe in `fleet.toml` is refused; rollout never guesses
//!   how to relaunch a process.
//! - SIGTERM first; SIGKILL after the configured grace period.
//! - `--window` samples the agorabus bus to guard the wm-* voice set.
//! - rollout never pushes git.

use std::process::ExitCode;

use clap::Parser;

mod cli;
mod error;
mod fleet;
mod health;
mod install;
mod restart;
mod scan;

use cli::{Cli, Command};
use error::RolloutError;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command.unwrap_or_default() {
        Command::Plan(args) => cli::run_plan(&args),
        Command::Apply(args) => cli::run_apply(&args),
        Command::Install(args) => cli::run_install_cmd(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rollout: {e}");
            ExitCode::FAILURE
        }
    }
}
