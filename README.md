# rollout — safe rolling restart for the live fleet

`binstale` tells you a daemon is running stale code. Bringing it current
is today a five-step hand-rolled dance — `cargo build --release` →
reinstall → `kill <pid>` → relaunch → `git push` — that the run-18
self-review deliberately *didn't* do autonomously because a careless
restart drops the live 8-peer `wm-*` voice fleet mid-conversation.
`rollout` makes that dance one command: it consumes `binstale scan`,
rebuilds/reinstalls/restarts stale daemons **one at a time**, polls
agorabus `peers` to confirm each one re-registers before moving on, and
defaults to `plan` (no mutation) so it shows the plan before touching
anything.

## Why a tool, not a script

1. The sequence is identical every time and error-prone by hand. A tool
   closes the "commit landed without the rebuild+restart" gap that
   re-staled the fleet at run 18.
2. The reason it's deferred is **safety**, not difficulty — dropping the
   voice fleet mid-turn is the real cost. A tool can encode the safety
   (serialize, confirm re-registration, window-guard) that a hand-typed
   `kill` cannot.

## Commands

- `rollout` / `rollout plan` — **default, non-mutating.** Print the
  ordered list of stale daemons and the exact build/install/launch
  commands that *would* run. Touches nothing.
- `rollout apply` — execute, **strictly serialized** (never two daemons
  in flight). Per daemon: build → install → record pre-restart peer set →
  SIGTERM the old pid → wait for exit (bounded grace, then SIGKILL
  fallback) → run `launch_cmd` → poll the healthcheck until the daemon
  re-registers on agorabus or a timeout elapses → emit a one-line result.
  Stops the whole run on the first daemon that fails to come back (does
  not cascade) and exits non-zero.
- `rollout install <binary> --dest <path>` — atomically install a
  freshly-built binary (temp-then-rename, mode 0755) and restart the
  owning systemd-user unit. Detects the backing unit by scanning
  `~/.config/systemd/user/*.service` `ExecStart=` lines; uses
  `agorabus reload --build` for `agorabus.service` and
  `systemctl --user restart` for every other daemon. Verifies the
  post-restart exe inode and honors `--dry-run`.

### Flags

- `--only <name>` — restrict the run to one daemon (e.g.
  `--only agorabus`).
- `--from -` — read `binstale scan --format json` from stdin instead of
  shelling out to `binstale`. `rollout` operates only on non-`fresh`
  verdicts.
- `--window <duration>` — **interim** voice-fleet safety guard. Refuses
  to restart any daemon whose name matches the voice set
  (`wm-dialog|stt|tts`) unless the bus has shown no `wm.dialog.turn.*`
  activity for `<duration>` (best-effort via a short agorabus subscribe
  sample). This is the coarse interim guard; the precise turn-in-flight
  guard is Fleet 2 (`rollout-window-guard`, depends on
  continuity-of-conversation's session-boundary events). Examples:
  `--window 30s`, `--window 2m`.

Every flag is documented in `rollout --help`, `rollout plan --help`,
`rollout apply --help`, and `rollout install --help`; `rollout --version`
prints `rollout 0.2.0`.

## Guarantees

- **Serialized-with-verify.** `apply` never has two daemon recipes
  mid-execution. Each daemon must re-register (healthcheck passes) before
  the next is touched; a failure stops the run rather than cascading.
- **No guessing how to relaunch.** A daemon with no entry in
  `fleet.toml` is refused with a clear error and is never killed;
  `rollout` exits non-zero listing the unknown daemons.
- **Plan-first posture.** `plan` is the default subcommand; `apply` /
  `install` are the only mutating paths and are never reached without an
  explicit subcommand.
- **rollout never pushes git.** It restarts *running* processes from
  *already-committed* source. Pushing is a separate human/skill concern
  (per the run-18 note and the /build commit+push convention), which
  keeps rollout's blast radius legible.

## `fleet.toml` schema

A launch-recipe config at `~/.config/rollout/fleet.toml` is **required** —
`rollout` refuses to restart a daemon it has no recipe for. Per daemon:

```toml
[daemons.agorabus]
repo        = "/home/jsy/wintermute/agorabus"   # optional cwd for the commands
build_cmd   = "cargo build --release"            # default
install_cmd = "install -Dm755 target/release/agorabus ~/.local/bin/agorabus"
launch_cmd  = "agorabus --observer &"
healthcheck = "agorabus peers | jq -e '.[] | select(.name==\"agorabus\")'"  # default agorabus peers check
grace_period_secs = 10                           # SIGTERM grace before SIGKILL fallback
```

## Install

```sh
git clone https://github.com/j0yen/rollout.git
cd rollout
cargo build --release
install -Dm755 target/release/rollout ~/.local/bin/rollout
```

Preview without mutating anything:

```sh
binstale scan --format json | rollout plan --from -
```

## License

Licensed under either of MIT or Apache-2.0 at your option.

Copyright (c) 2026 Joe Yen.
