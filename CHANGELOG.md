# Changelog

## v0.5.0 — 2026-06-13

turn-aware voice guard: subscribe to agorabus turn/session events, defer voice-daemon restarts mid-turn, extend VOICE_SET_PATTERN to include wm-audio

## v0.4.0 — 2026-06-13

Teach `rollout apply` to honour the `unit` field in `DaemonRecipe`: when a recipe carries a systemd unit name, restart via `systemctl --user restart <unit>` instead of SIGTERM+launch_cmd — matching how the live fleet is actually managed. Extracts shared `restart_unit` helper (pub(crate) in install.rs) so both `install` and `apply` call one implementation. Adds `RestartStrategy` enum and `restart_path` field on `RestartResult` for auditability. Legacy SIGTERM+launch_cmd path unchanged for non-systemd recipes.

## v0.3.0 — 2026-06-13

Add rollout fleet-gen subcommand to derive fleet.toml.proposed from live binstale+systemd state; adds unit field to DaemonRecipe.

## Unreleased

Add integration tests proving acceptance criteria 2, 3, 5, 6, and 7
(`tests/acceptance_ac2_3_5_6_7.rs`): `apply --only` restart-with-new-pid,
strict one-at-a-time serialization, stop-on-healthcheck-failure (no cascade),
SIGTERM-then-SIGKILL escalation against an uncooperative fixture, and the
`--window` voice-set guard (blocks on bus activity, allows when quiet) via a
stub `agorabus` on PATH. All previously-implemented-but-unproven ACs are now
green.

## v0.2.0 — 2026-06-02

Add `rollout install <binary> --dest <path>`: atomic binary install + systemd-user daemon restart.
Scans ~/.config/systemd/user/*.service ExecStart= lines to find the owning unit, uses
`agorabus reload --build` for agorabus.service, `systemctl --user restart` for all other daemons.
Includes voice-set window guard, post-restart exe-inode verification, and --dry-run mode.
Closes the "install and restart are decoupled" gap for recalld, wmd, wm-audio, wm-dialog/stt/tts.
