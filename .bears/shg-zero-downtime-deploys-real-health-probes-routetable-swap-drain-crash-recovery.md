---
id: shg
title: "Zero-downtime deploys: real health probes, RouteTable swap, drain, crash recovery"
status: open
priority: P1
created: "2026-11-07T14:45:17.296793894Z"
updated: "2026-11-07T21:35:20.244898667Z"
tags:
  - orchestrator
depends_on:
  - gnz
  - "7na"
  - pav
  - u3t
parent: mdd
---

## Summary
Complete the start-first path of the state machine built in `8w4` (architecture §5 / ADR-0017) by replacing its degenerate effects: `HealthChecking` runs the configured probes (`gnz`), `Switching` has the orchestrator publish a new RouteTable (`pav`), `Draining` waits on the old upstream's inflight counter (`7na`). The transition function itself should need no structural change; if it does, fix `8w4`'s design rather than forking it.

## Acceptance Criteria
- [ ] New generation runs alongside old on fresh ports; swap performed by the orchestrator publishing RouteTable; the durable per-service current pointers (one transaction) are the commit point
- [ ] Drain via inflight counters with stack `drain_timeout` and the 1 s settle delay (ADR-0035); then stop+remove old, release ports
- [ ] Any failure before Switching → Aborting → Failed with old generation still serving
- [ ] Order per ADR-0048: Starting/HealthChecking cover start-first services, `Replacing` covers stop-first services (`v2h`), one Switching commits the whole stack
- [ ] Crash recovery per architecture §5.4 implemented in the reconciler (`tsm`), using per-service pointers

## Testing
- Existing transition-function tests from `8w4` still pass unchanged
- FakeRuntime scenario tests incl. crash at each state + restart
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §5
- ADR-0017, ADR-0039, ADR-0048