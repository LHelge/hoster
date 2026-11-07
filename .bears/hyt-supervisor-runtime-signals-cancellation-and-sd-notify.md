---
id: hyt
title: "Supervisor: runtime, signals, cancellation and sd_notify"
status: in_progress
priority: P1
created: "2026-11-07T14:45:17.158461669Z"
updated: "2026-11-07T22:29:45.474847799Z"
tags:
  - core
depends_on:
  - "26p"
parent: "8xd"
attempts: 1
---

## Summary
Implement the supervisor that owns the orchestrator tokio runtime, OS signals and the startup/shutdown ordering skeleton from architecture §4, with systemd notify integration.

## Acceptance Criteria
- [ ] `hoster run` builds a multi-thread tokio runtime and a root `CancellationToken`
- [ ] SIGTERM → graceful shutdown sequence (stubs for each phase, logged); second SIGTERM or SIGINT → fast shutdown
- [ ] `sd_notify` READY=1 after startup phases, STOPPING=1 on shutdown (no-op when not under systemd)
- [ ] A `Phase` enum / startup sequence function that later tasks plug into, mirroring architecture §4 steps 1–9
- [ ] Exit codes: 0 clean, non-zero on startup failure with a clear message

## Implementation Notes
- Use `tokio::signal::unix`; `sd-notify` crate
- Keep a placeholder for the Pingora thread handle and the `pingora_shutdown` watch channel (architecture §3.3 #5)

## Testing
- Integration test: spawn the binary, send SIGTERM, assert clean exit within timeout
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §3, §4