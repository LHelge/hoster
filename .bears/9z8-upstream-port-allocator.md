---
id: "9z8"
title: Upstream port allocator
status: done
priority: P2
created: "2026-11-07T14:45:17.184860490Z"
updated: "2026-11-07T22:46:57.928395269Z"
tags:
  - store
depends_on:
  - h3a
  - pu4
parent: "5ch"
attempts: 1
---

## Summary
Allocate and release 127.0.0.1 host ports for routed services from the configured range, persisted in the store, per ADR-0026.

## Acceptance Criteria
- [ ] allocate(stack, service, container_port, generation) → port; release(port); list
- [ ] Round-robin: next free port after the last one handed out, wrapping; default range 20000–29999
- [ ] Ports marked bad on 'address in use' and skipped for 10 minutes
- [ ] Allocation survives restart; exhaustion returns a typed error used by deploy validation

## Edge Cases & Considerations
- Concurrent allocations for two stacks never return the same port

## Testing
- Unit tests: exhaustion, release/reuse ordering, persistence across reopen, concurrency
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0026
- ADR-0007