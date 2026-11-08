---
id: u92
title: "install.sh: machinectl missing on Ubuntu; offer a working hoster shell"
status: open
priority: P2
created: "2026-11-08T11:58:34.765671407Z"
updated: "2026-11-08T11:58:34.765671407Z"
tags:
  - ops
  - docs
---

## Summary
install.sh ends with `sudo machinectl shell hoster@`, but `machinectl` is in `systemd-container`, which Ubuntu doesn't install by default. Found on the first real install (Ubuntu 26.04).

## Acceptance Criteria
- [ ] install.sh installs `systemd-container` with apt (and checks dnf's equivalent) alongside podman, or prints the `sudo -iu hoster` + `export XDG_RUNTIME_DIR=/run/user/$(id -u)` variant when machinectl is missing
- [ ] docs/install.md's "A shell as hoster" covers both
- [ ] scripts/test-install.sh covers the chosen behaviour

## References
- ADR-0052 (Install script), ADR-0023
- install.sh, docs/install.md