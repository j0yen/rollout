# Changelog

## v0.11.0 — 2026-06-14

changeover-activate: rollout cycle subcommand (prove→apply-warmswap→verify) + dormant systemd timer; ROLLOUT_AUTO_ENABLED=0 default, dry-run only until user enables

## v0.10.0 — 2026-06-13

`rollout cycle` — automated prove → apply (warm-swap only) → verify loop with dormant
systemd timer. The `cycle` subcommand runs `rollout prove --all` to refresh the proof
ledger, then `rollout apply --auto` restricted to warm-swap daemons only (no
hard-restart fallback; a daemon with no warm-swap path is **skipped**, not restarted),
then post-swap verification asserting each rolled daemon re-holds its agorabus claim.
A JSON receipt is written to `~/.local/state/rollout/receipts/` after every run.

**Ships DORMANT**: `ROLLOUT_AUTO_ENABLED=0` by default; `changeover-activate.timer`
ships but is NOT enabled. To activate:
```
ROLLOUT_AUTO_ENABLED=1 systemctl --user enable --now changeover-activate.timer
```
Requires `PRD-rollout-selfreview-apply.md` (user-gated, blocked) for the policy unlock.
This PRD ships the capability dormant and does not take that step.

## v0.9.0 — 2026-06-13

Adds `rollout prove --daemon <unit>` / `--all` / `--dry-run` subcommand that
invokes `changeover probe` and seeds ~/.config/rollout/proofs.json — closing
the gap where `apply --auto` refused everything because the ledger never existed.
Includes a daily systemd-user timer (changeover-prove.{timer,service}) and
5 fixture-based cargo tests. Version bumped to 0.8.0.

## v0.8.0 — 2026-06-13

`rollout prove` — one-shot proof seeder: new subcommand runs `changeover probe <unit> --json`
and feeds the output through the existing `record-proof` ingestion path into
`~/.config/rollout/proofs.json`. `--daemon <unit>` proves a single daemon; `--all` proves
every daemon in `fleet.toml`; `--dry-run` prints without writing. Exits non-zero on any Refuse
verdict (events_lost > 0 or binary hash mismatch). Adds `FleetConfig::all_names()` to enumerate
the fleet for `--all`. New `contrib/systemd/changeover-prove.{timer,service}` install under
`~/.config/systemd/user/` for a daily automated re-proof; `systemctl --user --dry-run enable`
safe (prove is read+measure-only). Fixture-based `cargo test` covers ACs 1–4 and 6.

## v0.7.0 — 2026-06-13

Proof ledger and `rollout apply --auto`: new `autogate` module with `ProofLedger`, `ProofEntry`, `GateConfig`, and `gate()` (pure, no I/O). New `rollout record-proof --from <probe-json>` subcommand ingests `changeover probe` output into `~/.config/rollout/proofs.json`. `rollout apply --auto` consults the ledger per-daemon and skips any with a Refuse verdict (no matching binary hash or events lost > 0); requires `ROLLOUT_AUTO_ENABLED=1` interlock. `rollout apply --dry-run` prints the plan without restarting. Comprehensive unit tests cover all gate branches (AC3–AC5).

## v0.6.0 — 2026-06-13

warm-swap restart strategy: start successor first on agorabus ClaimAcquire, stop predecessor after successor holds the lease, then verify exactly one holder — zero-loss subscribe window for the live fleet. plan output gains a strategy column (warm-swap|hard).

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
