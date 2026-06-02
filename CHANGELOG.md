# Changelog

## v0.2.0 — 2026-06-02

Add `rollout install <binary> --dest <path>`: atomic binary install + systemd-user daemon restart.
Scans ~/.config/systemd/user/*.service ExecStart= lines to find the owning unit, uses
`agorabus reload --build` for agorabus.service, `systemctl --user restart` for all other daemons.
Includes voice-set window guard, post-restart exe-inode verification, and --dry-run mode.
Closes the "install and restart are decoupled" gap for recalld, wmd, wm-audio, wm-dialog/stt/tts.
