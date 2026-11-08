---
id: fcs
title: Structured logging, proxy access log, deploy event log
status: in_progress
priority: P2
created: "2026-11-07T14:45:17.359289409Z"
updated: "2026-11-07T23:52:10.771413446Z"
tags:
  - observability
depends_on:
  - wks
  - ej8
parent: "6gn"
attempts: 1
---

## Summary
Consistent structured logs for hoster itself and an access log from the proxy's `logging` hook.

## Acceptance Criteria
- [ ] Optional JSON log format; span fields stack/service/generation/deployment_id
- [ ] Access log per request: host, route, upstream, status, duration, bytes, client IP (from `logging` hook)
- [ ] Deployment events visible in the deployment page (already persisted) and logs

## Testing
- Unit tests on log formatting; access-log hook test
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0044, ADR-0045