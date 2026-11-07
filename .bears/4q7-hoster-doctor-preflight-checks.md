---
id: "4q7"
title: "`hoster doctor` preflight checks"
status: open
priority: P2
created: "2026-11-07T14:45:17.161104548Z"
updated: "2026-11-07T21:12:34.669057181Z"
tags:
  - core
  - ops
depends_on:
  - "26p"
parent: "8xd"
---

## Summary
A preflight subcommand that verifies the host is set up per ADR-0023 (user service, linger, sysctl), so misconfiguration is diagnosed before `run`.

## Acceptance Criteria
- [ ] Checks and reports pass/fail with remediation hints: config parses; state dir exists and is writable with correct perms; podman socket path exists and answers `/_ping` (plain HTTP over UDS, no bollard needed yet); `net.ipv4.ip_unprivileged_port_start` ≤ 80 and can bind :80 and :443 (bind + immediately close); linger enabled (`loginctl show-user $USER -p Linger`); user `podman.socket` enabled/active; whether user-scoped encrypted credentials are available (systemd ≥ 256) for the secret-store key (ADR-0029); configured port range does not overlap `net.ipv4.ip_local_port_range`; system clock NTP-synchronized (`timedatectl`), needed for OIDC token expiry checks (ADR-0047)
- [ ] Exit code non-zero if any required check fails; `--json` output option
- [ ] Runs as the service user without root

## Implementation Notes
- Socket path: `$XDG_RUNTIME_DIR/podman/podman.sock` then `/run/user/$UID/podman/podman.sock` (same order bollard uses)

## Testing
- Unit tests for each check with injected paths/fakes
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0023
- docs/architecture.md §1.3