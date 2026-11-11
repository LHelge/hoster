---
id: gyy
title: Raise RLIMIT_NOFILE and refuse new clients beyond the total descriptor cap
status: open
priority: P1
created: "2026-11-11T21:55:08.882684187Z"
updated: "2026-11-11T21:55:08.882684187Z"
tags:
  - proxy
  - security
parent: "6sq"
---

## What
ADR-0065 total cap.
- At startup, raise the soft `RLIMIT_NOFILE` to `min(16384, hard)`, logging the result. The unit also gets `LimitNOFILE=16384` (`deploy/systemd/hoster.service`; `scripts/check-systemd-units.sh` and `scripts/test-systemd-unit.sh` still pass).
- Enable pingora's `connection_filter` feature (`cargo add`, don't hand-edit versions) and set one `ConnectionFilter` on every proxy listener (`:80`, `:443`). `should_accept` refuses while the process has ≥ 4096 open descriptors (`/proc/self/fd`), or a quarter of the limit if that is lower. Keep the counting cheap: a cached count refreshed at most every N ms is fine, as long as a flood cannot overshoot by much.
- Count refusals. Log at most once per interval while refusing, and expose the count where the System page can show it.
- Constants, not config.

## Acceptance
- [ ] A test opens connections up to the cap and sees the next one refused, while an existing connection and the admin host keep working (`src/proxy/tests/limits.rs`)
- [ ] `tests/supervisor.rs` (or equivalent) proves hoster's own descriptor work (SQLite, a podman call) still works at the cap
- [ ] Architecture §9.1 "Many connections" row updated, plus the "File descriptors" note
- [ ] `hoster doctor` or startup log shows the effective limit

## References
- ADR-0065; `docs/architecture.md` §9.1; `src/proxy/mod.rs`, `src/proxy/tests/limits.rs`, `deploy/systemd/hoster.service`
- pingora-core 0.9.1 `src/listeners/connection_filter.rs`