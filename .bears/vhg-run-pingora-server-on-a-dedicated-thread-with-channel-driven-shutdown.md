---
id: vhg
title: Run Pingora Server on a dedicated thread with channel-driven shutdown
status: done
priority: P1
created: "2026-11-07T14:45:17.237378955Z"
updated: "2026-11-07T23:01:49.773892494Z"
tags:
  - proxy
depends_on:
  - hyt
parent: up4
attempts: 1
---

## Summary
Embed Pingora 0.9.x: build `Server` with a code-constructed `ServerConf`, run it on a `std::thread`, control shutdown via our own `ShutdownSignalWatch`, and use `watch_execution_phase()` for readiness (architecture §1.1, §3).

## Acceptance Criteria
- [ ] `pingora` = 0.9 with features `proxy`, `rustls`; `ServerConf`: daemon=false, threads from config, explicit grace_period_seconds / graceful_shutdown_timeout_seconds
- [ ] `ChannelShutdownWatch` implements `ShutdownSignalWatch` over `watch::Receiver<Option<ShutdownSignal>>`
- [ ] Supervisor spawns the thread, waits for `ExecutionPhase::Running` (timeout → startup failure), and on shutdown sends GracefulTerminate then joins
- [ ] Never calls `run_forever`; no Pingora signal handlers installed
- [ ] A temporary plain-HTTP service on a configurable address returns 404 for everything (real proxy comes next)

## Edge Cases & Considerations
- Bind failure on the listen address → Pingora terminates; supervisor detects early Terminated/thread exit and exits non-zero

## Testing
- Integration test: start supervisor on an ephemeral port, request → 404, SIGTERM → clean exit within grace period
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §1.1, §3, §4
- ADR-0008