---
id: "7uz"
title: "`hoster update` and `hoster rollback` CLI subcommands"
status: open
priority: P1
created: "2026-11-08T08:25:49.995852108Z"
updated: "2026-11-08T08:25:55.875484042Z"
tags:
  - core
  - ops
depends_on:
  - wpm
parent: tbf
---

## Summary
Expose the updater on the command line for SSH sessions as the hoster user.

## Acceptance Criteria
- [ ] `hoster update [--version vX.Y.Z] [--check]`: `--check` prints current/latest and the release notes and exits; otherwise prints the notes, asks for confirmation (`--yes` to skip), runs the update and restarts the service via `systemctl --user restart hoster`
- [ ] `hoster rollback [--yes]`: shows current → previous version and how many migrations will be undone (and that their data is lost), asks for confirmation, runs the rollback
- [ ] Both refuse to run as root, with a hint to use `sudo -iu hoster` / `machinectl shell hoster@`
- [ ] Exit codes: 0 success / already up to date, 1 error, 2 usage; errors print the updater's message without a backtrace
- [ ] docs: the operating table in docs/install.md (or its replacement) lists both commands

## Testing
- CLI parsing tests; command tests over the updater's fakes

## References
- ADR-0052 (Self-update)
- src/main.rs