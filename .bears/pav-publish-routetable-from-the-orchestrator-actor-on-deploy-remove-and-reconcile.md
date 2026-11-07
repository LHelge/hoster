---
id: pav
title: Publish RouteTable from the orchestrator actor on deploy, remove and reconcile
status: open
priority: P1
created: "2026-11-07T14:45:17.245274308Z"
updated: "2026-11-07T21:44:41.392310037Z"
tags:
  - orchestrator
  - routing
depends_on:
  - wks
  - "8w4"
  - tsm
parent: up4
---

## Summary
Make the orchestrator the single writer of `ArcSwap<RouteTable>` (ADR-0050): rebuild from in-memory desired state and `store` after deploys, removals and reconciliation (architecture §6.4), always under the orchestrator's internal lock and never from a deploy task.

## Acceptance Criteria
- [ ] Orchestrator keeps DesiredState (per-service current generation, upstreams and routes per stack) in memory, restored at startup
- [ ] RouteTable published before the Pingora thread starts (startup step 6) so the first request sees routes
- [ ] `transition()` on `Switching` publishes the table with every changed service's new upstream; after RemoveStack its routes disappear; `Replacing` publishes `Unavailable` for the service in its gap (ADR-0048)
- [ ] Each publish logs version + diff summary
- [ ] The publish happens inside the lock-held, non-awaiting section; the store write for the pointers follows in the same `transition` call

## Testing
- Unit test: two stacks deploying concurrently never lose each other's routes (FakeRuntime, deterministic interleaving)
- Gated end-to-end: deploy example stack, curl through the proxy with Host headers for /, /api, s3 host
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §6.4
- ADR-0035, ADR-0048, ADR-0050