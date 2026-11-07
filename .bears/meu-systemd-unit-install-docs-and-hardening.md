---
id: meu
title: systemd unit, install docs and hardening
status: in_progress
priority: P1
created: "2026-11-07T14:45:17.163730705Z"
updated: "2026-11-07T22:38:23.065807544Z"
tags:
  - ops
  - docs
depends_on:
  - "4q7"
  - t94
parent: "8xd"
attempts: 1
---

## Summary
Ship the systemd **user** unit per ADR-0023 (user service + linger + `ip_unprivileged_port_start=80`) plus an install guide.

## Acceptance Criteria
- [ ] `deploy/systemd/hoster.service` (user unit): Type=notify, Restart=always, Requires/After=podman.socket, WantedBy=default.target, TimeoutStopSec greater than the configured Pingora grace period; `deploy/sysctl/50-hoster.conf`
- [ ] Best-effort hardening available to user units (NoNewPrivileges, PrivateTmp, ProtectSystem where supported unprivileged); document what is not available
- [ ] `docs/install.md`: create user, `loginctl enable-linger hoster`, sysctl, `systemctl --user enable --now podman.socket` as hoster (e.g. via `machinectl shell hoster@`), install binary/unit/config, set HOSTER_ADMIN_GITHUB (unit `Environment=`), run `hoster doctor`, start
- [ ] Manually verified on a VM or the dev host: unit starts at boot without login, `systemctl --user stop hoster` exits cleanly, `journalctl --user -u hoster` shows logs

## Implementation Notes
- `systemd-analyze security hoster.service` output noted in the PR

## Edge Cases & Considerations
- podman.socket not yet active at boot → hoster waits with timeout and fails with a clear message

## Testing
- `systemd-analyze verify` on the unit files in CI
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0023
- ADR-0001