---
id: "7na"
title: Per-upstream in-flight counters and drain helper
status: open
priority: P1
created: "2026-11-07T14:45:17.292369144Z"
updated: "2026-11-07T14:45:17.292369144Z"
tags:
  - proxy
depends_on:
  - wks
parent: mdd
---

## Summary
Track in-flight requests per Upstream with an RAII guard in the proxy CTX and provide a drain-wait helper (architecture §3.2, §5.3).

## Acceptance Criteria
- [ ] Guard increments on upstream selection and decrements on drop (covers success, error, client abort)
- [ ] `drain(upstream, settle_delay, timeout)` waits for inflight == 0 after a settle delay; returns Drained or TimedOut(count)
- [ ] Counter exposed for metrics later

## Testing
- Integration test: slow upstream, swap table, verify drain waits for the in-flight request and completes after it
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §3.2, §5.3