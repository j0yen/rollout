# rollout — safe rolling restart for a live daemon fleet

`rollout` brings stale daemons current one at a time, confirms each one comes back before touching the next, and shows you the plan before it changes anything.

## The problem

`binstale` can tell you a daemon is running stale code. Acting on that is the hard part. By hand it's a five-step dance — build, reinstall, `kill <pid>`, relaunch, confirm — and the danger isn't the typing, it's the blast radius: a careless restart drops a live voice fleet mid-conversation. So the restart gets deferred, the daemon stays stale, and the gap between "commit landed" and "fleet is running it" stays open.

The sequence is identical every time, which makes it a tool, not a judgment call. And the reason it was deferred is *safety*, not difficulty — which means a tool can encode the safety a hand-typed `kill` cannot: serialize the restarts, confirm re-registration before proceeding, and refuse to touch a daemon mid-turn.

## Install

```sh
git clone https://github.com/j0yen/rollout.git
cd rollout
cargo build --release
install -Dm755 target/release/rollout ~/.local/bin/rollout
```

## Quickstart

`plan` is the default subcommand and mutates nothing. Pipe `binstale` into it to see exactly what `apply` would do:

```sh
binstale scan --format json | rollout plan --from -
```

It prints the ordered list of stale daemons, each with the build/install/launch commands and the restart strategy that would run. When you're ready, swap `plan` for `apply`.

## Commands

- **`rollout` / `rollout plan`** — default, non-mutating. Print the ordered restart plan; touch nothing. A daemon with no recipe in `fleet.toml` fails the plan up front rather than being killed later.
- **`rollout apply`** — execute, strictly serialized: never two daemons in flight. Per daemon — build → install → restart → poll the healthcheck until it re-registers on agorabus (or times out) → one-line result. The first daemon that fails to come back stops the run and exits non-zero; failures don't cascade. Voice-set daemons (`wm-dialog|stt|tts`) are deferred, never restarted blind, when the bus shows turn activity or is unreachable.
- **`rollout install <binary> --dest <path>`** — atomically install a freshly-built binary (temp-then-rename, mode 0755) and restart the systemd-user unit whose `ExecStart` points at that dest. Uses `agorabus reload --build` for agorabus and `systemctl --user restart` otherwise; verifies the post-restart exe.
- **`rollout fleet-gen`** — derive a candidate `fleet.toml` from the live daemon set by cross-referencing `binstale` against `~/.config/systemd/user/*.service`. Writes a `.proposed` file for review; never writes `fleet.toml` directly.
- **`rollout prove` / `rollout record-proof`** — seed and update the per-daemon proof ledger (`~/.config/rollout/proofs.json`) from `changeover probe` output. The ledger is what `apply --auto` consults.
- **`rollout cycle`** — automated prove → apply (warm-swap only) → verify loop, gated by `ROLLOUT_AUTO_ENABLED`. Unset or `0` (the default), it runs dry: probe, plan, would-verify, zero restarts.

Every flag is documented in `--help` for each subcommand. `rollout --version` prints the crate version.

## How it works

Two restart strategies:

- **Hard restart** — SIGTERM the old pid, wait a bounded grace period (SIGKILL fallback), relaunch, then poll the healthcheck. There's a brief window where no instance holds the daemon's claim.
- **Warm-swap** — start the successor *first* via `systemd-run --user --scope`, wait for it to contend for the daemon's agorabus claim, stop the predecessor, then confirm exactly one holder remains. No window where the claim is unheld. `cycle` uses warm-swap only and skips any daemon that has no warm-swap path rather than hard-restarting it.

Three standing guarantees:

- **Serialized with verify.** `apply` never has two daemon recipes mid-execution; each must re-register before the next is touched.
- **No guessing how to relaunch.** A daemon with no `fleet.toml` recipe is refused with a clear error and never killed.
- **rollout never pushes git.** It restarts *running* processes from *already-committed* source. Pushing stays a separate, human-gated step — which keeps rollout's blast radius legible.

## `fleet.toml`

A launch-recipe config at `~/.config/rollout/fleet.toml` is required; `rollout` refuses any daemon it has no recipe for. Per daemon:

```toml
[daemons.agorabus]
repo        = "/home/jsy/wintermute/agorabus"   # optional cwd for the commands
build_cmd   = "cargo build --release"
install_cmd = "install -Dm755 target/release/agorabus ~/.local/bin/agorabus"
launch_cmd  = "agorabus --observer &"
healthcheck = "agorabus peers | jq -e '.[] | select(.name==\"agorabus\")'"
grace_period_secs = 10                           # SIGTERM grace before SIGKILL fallback
```

`rollout fleet-gen` produces a first draft of this file from live state.

## Status

Used to restart the wintermute daemon fleet. The `cycle` auto-path ships dormant: `ROLLOUT_AUTO_ENABLED` defaults off and the `changeover-activate` timer is installed but not enabled, so unattended live restarts require a deliberate opt-in.

## License

Licensed under MIT OR Apache-2.0 at your option.

Copyright (c) 2026 Joe Yen.
