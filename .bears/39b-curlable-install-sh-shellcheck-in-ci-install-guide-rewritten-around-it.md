---
id: "39b"
title: Curlable install.sh, shellcheck in CI, install guide rewritten around it
status: open
priority: P1
created: "2026-11-08T08:25:50.020301649Z"
updated: "2026-11-08T08:25:50.020301649Z"
tags:
  - ops
  - docs
depends_on:
  - a5g
  - vug
parent: tbf
---

## Summary
`curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh | sudo bash` sets up a host from scratch or re-runs safely, per ADR-0052.

## Acceptance Criteria
- [ ] `install.sh` at the repo root (bash, `set -euo pipefail`), published as a release file by release.yml; flags `--version vX.Y.Z`, `--dry-run` (print every action, change nothing), `--help`
- [ ] The whole script is wrapped in a function called on the last line, so a truncated download runs nothing
- [ ] Root part:
  - refuses unless root on Linux x86_64/aarch64 with systemd
  - installs podman with apt or dnf if missing, else a clear error naming the package
  - creates the `hoster` system user with home and subuid/subgid ranges (skip if present)
  - `loginctl enable-linger hoster`
  - installs `50-hoster.conf` to /etc/sysctl.d and applies it
- [ ] User part (as hoster, with a working user bus):
  - downloads the binary for `uname -m` and `SHA256SUMS` from the chosen release (latest stable by default) and verifies the checksum, aborting on mismatch
  - installs into `~/.local/lib/hoster/versions/vX.Y.Z` and switches `current`
  - enables `podman.socket`
  - installs/updates the unit and runs `daemon-reload`
  - creates state/config dirs (0700) and writes the example config only if none exists
  - runs `hoster doctor`
- [ ] Never starts or restarts the service; ends with the remaining manual steps (GitHub handle via `systemctl --user edit`, OAuth App, `admin_host`, ACME contact) and the start command
- [ ] Re-running on an installed host changes nothing except installing a newer version if requested
- [ ] CI runs shellcheck on install.sh and all scripts/
- [ ] docs/install.md rewritten: one-line install first, what the script does, manual steps, update/rollback (UI, `hoster update`, `hoster rollback`), manual recovery (restore pre-update snapshot + repoint `current`), and the accepted risks from ADR-0052; README quick start points at it

## Testing
- shellcheck clean; `--dry-run` output reviewed
- Run in a disposable systemd-enabled container or VM (e.g. podman `--systemd=always` Debian/Fedora image) against a locally served fake release dir; document how (env var to override the release base URL, used only for testing)

## References
- ADR-0052 (Install script)
- ADR-0023
- docs/install.md, deploy/