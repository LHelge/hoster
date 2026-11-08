---
id: v2h
title: "stop-first: Replacing step, proxy gap (503 + Retry-After), abort restores old container"
status: done
priority: P2
created: "2026-11-07T14:45:17.301373014Z"
updated: "2026-11-08T02:21:07.680100462Z"
tags:
  - orchestrator
depends_on:
  - shg
  - u3t
parent: mdd
attempts: 1
---

## Summary
Implement the `Replacing` step of ADR-0048 for `update = "stop-first"` services (ADR-0039): per service, publish `Unavailable` for its routes, stop the old container (keep it), start the new one, probe it. The step runs after every start-first service is healthy and before the single stack-wide swap. The state and its transitions already exist in the pure function from `8w4`; this task adds the proxy-facing effects and the abort path.

## Acceptance Criteria
- [ ] `Replacing` processes changed stop-first services one at a time; before stopping the old container the orchestrator publishes a RouteTable in which that service's routes have `Target::Unavailable { retry_after }` (architecture §6.1, §6.4)
- [ ] Proxy returns 503 + `Retry-After` for `Unavailable` targets
- [ ] On any failure before Switching (including a start-first failure after Replacing began — not possible by ordering, but a stop-first failure after another stop-first service was replaced is), `Aborting` restarts every stopped old container, restores its routes, and removes all new containers; FakeRuntime test asserts every service is served by exactly one generation afterwards
- [ ] Stopped old stop-first containers are removed in `CleaningUp`, not in `Replacing`
- [ ] Deployment with no stop-first changes passes through `Replacing` without effects

## Testing
- FakeRuntime tests: single stop-first; two stop-first where the second fails → first restarted; mixed stack happy path commits all services in one swap
- Proxy integration test asserting 503 during the gap and 200 after
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0048, ADR-0039, ADR-0032