---
id: "98v"
title: Continuous health monitoring and status
status: open
priority: P3
created: "2026-11-07T14:45:17.374526819Z"
updated: "2026-11-07T14:45:17.374526819Z"
tags:
  - observability
  - orchestrator
depends_on:
  - gnz
  - tsm
parent: "6gn"
---

## Summary
Run liveness probes on live services and surface status (no auto-restart beyond ADR-0028 reconcile).

## Acceptance Criteria
- [ ] Periodic probes per service using its health config; status Healthy/Unhealthy/CrashLooping
- [ ] Status changes emitted on `ui_events` and metrics

## Testing
- Unit tests with paused time
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0038, ADR-0028