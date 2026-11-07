---
id: "25t"
title: Per-stack deploy queue, coalescing and cancellation
status: open
priority: P2
created: "2026-11-07T14:45:17.305987152Z"
updated: "2026-11-07T14:45:17.305987152Z"
tags:
  - orchestrator
depends_on:
  - shg
parent: mdd
---

## Summary
Implement deploy concurrency per ADR-0040.

## Acceptance Criteria
- [ ] One active deployment per stack; newest pending target kept, older ones recorded as Superseded
- [ ] Queued target re-checked for idempotency when it starts
- [ ] Cancel before Switching → Aborting
- [ ] Global limit on concurrent pulls

## Testing
- Unit tests with controlled FakeRuntime latencies
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0040