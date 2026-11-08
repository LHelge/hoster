---
id: pap
title: Stack removal
status: done
priority: P2
created: "2026-11-07T14:45:17.214788036Z"
updated: "2026-11-08T00:30:51.998981942Z"
tags:
  - orchestrator
depends_on:
  - "8w4"
parent: ame
attempts: 1
---

## Summary
RemoveStack command: stop and remove containers, network, port allocations and routes; keep volumes unless explicitly requested.

## Acceptance Criteria
- [ ] RemoveStack{stack, delete_volumes: false} removes containers, network, allocations, store rows (history retained)
- [ ] delete_volumes=true requires an explicit confirmation token from the caller
- [ ] Removing a stack mid-deploy aborts the deploy first

## Testing
- FakeRuntime unit tests; gated integration test
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0030