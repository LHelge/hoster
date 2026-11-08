---
id: vug
title: Versioned binary layout under ~/.local/lib/hoster, unit change, `hoster version --json`
status: done
priority: P1
created: "2026-11-08T08:25:15.972675075Z"
updated: "2026-11-08T09:28:52.351992371Z"
tags:
  - core
  - ops
parent: tbf
attempts: 1
---

## Summary
Move the binary from `/usr/local/bin/hoster` to the versioned layout from ADR-0052 and give hoster a typed model of it, plus a machine-readable `version` subcommand.

## Acceptance Criteria
- [ ] Layout type (e.g. `Installation`) rooted at `~/.local/lib/hoster`: `versions/vX.Y.Z/hoster`, `current -> versions/vX.Y.Z`; operations: list installed versions, current, previous, install a binary file as a version (write to temp, fsync, chmod 0755, rename), atomic switch (new symlink + `rename(2)` over `current`), prune to current + previous. Fully unit-tested on a temp dir
- [ ] `hoster version` prints the version; `hoster version --json` prints `{"version":"X.Y.Z","target":"<triple>","schema":<latest migration version>}`; `hoster --version` still works
- [ ] `deploy/systemd/hoster.service`: `ExecStart=%h/.local/lib/hoster/current/hoster run`; `ReadWritePaths` gains `%h/.local/lib/hoster`; `scripts/check-systemd-units.sh` updated for the new ExecStart and still passes
- [ ] `hoster doctor` reports when the running binary is not inside the layout (warning, not failure), so manual installs still work
- [ ] No remaining `/usr/local/bin/hoster` references outside docs/install.md (that file is rewritten by the install.sh task)

## Implementation Notes
- Resolve the layout root from `$HOME`/XDG consistently with config.rs's state/config dir handling.
- The schema number comes from the Store API added by the reversible-migrations task.

## Testing
- Unit tests for the layout type (install, switch, prune, previous with 1/2/3 versions, dangling `current`)
- CLI test for `version --json`

## References
- ADR-0052 (Binary layout)
- ADR-0023
- docs/architecture.md §10