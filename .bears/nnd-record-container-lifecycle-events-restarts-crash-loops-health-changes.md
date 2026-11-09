---
id: nnd
title: "Record container lifecycle events: restarts, crash loops, health changes"
status: open
priority: P2
created: "2026-11-09T14:05:07.896514310Z"
updated: "2026-11-09T14:05:07.896514310Z"
tags:
  - orchestrator
  - runtime
  - observability
depends_on:
  - nbj
parent: "4qj"
---

## Summary
Make ADR-0028's "restarts are recorded as events" true. The reconciler and the health monitor write container lifecycle events (ADR-0055 §2).

## Acceptance Criteria
- [ ] Restart: written per reconciler restart, with the service, the generation, the exit code and **OOM** when podman's inspect reports `OOMKilled`. Severity `Warning`
- [ ] Crash loop entered (`Error`) and left (`Success`). Entered is the transition the restart backoff already reports; left is when the restart history resets
- [ ] Health change of a live service (the existing `UiEvent::Health` transitions), written with the probe detail. A flapping service must not flood the log: write only real transitions, which the monitor already distinguishes
- [ ] The startup reconcile (starting the current generation after boot) does not write one restart event per container; it is covered by the "hoster started" event (operator/system task)

## Implementation Notes
- `src/orchestrator/reconcile.rs` (restart backoff state machine ~line 66), `src/orchestrator/monitor.rs`
- Exit code/OOM: from the container inspect the reconciler already does, via `src/runtime/`

## Testing
- `reconcile_tests.rs` / `monitor_tests.rs`: restart writes one event, crash loop enter/leave, no events for startup reconcile
- `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0055, ADR-0028, ADR-0038
- `src/orchestrator/reconcile.rs`, `src/orchestrator/monitor.rs`, `src/runtime/`