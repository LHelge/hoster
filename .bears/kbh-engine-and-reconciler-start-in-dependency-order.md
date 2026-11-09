---
id: kbh
title: "Engine and reconciler: start in dependency order"
status: open
priority: P1
created: "2026-11-09T17:23:25.583018503Z"
updated: "2026-11-09T19:11:06.557668828Z"
tags:
  - orchestrator
depends_on:
  - rxt
parent: qq9
---

## Summary
Implement ADR-0056 Decisions 2 and 3 in the deploy engine and the reconciler. A service starts as soon as its own dependencies have passed: there are no strict waves, which ADR-0056 rejects. ADR-0048's atomic commit stays intact.

## Acceptance Criteria
- [ ] **Start phase** (`src/orchestrator/engine.rs`, `Plan` in `plan.rs`): the changed start-first services plus the changed stop-first services without a previous generation. `Plan` exposes the phase classification and which services are held back; the engine doesn't re-derive them. `Starting` starts the services with no in-phase dependency. `HealthChecking` starts each further one as soon as its in-phase dependencies pass, and probes run in parallel.
- [ ] **Held back:** a start-phase service that depends, even transitively, on a replace-phase service is started and probed inside `Replacing`, once that dependency has passed.
- [ ] **Replace phase:** one at a time, in dependency order with name as the tie-break.
- [ ] **Unchanged dependencies:**
  - They must be running at the start and before each held-back or replaced dependent starts, otherwise the deploy fails with "db (unchanged) is not running". Their monitor status is logged, not awaited.
  - An unchanged service that was running at the start and exited while its replaced dependency was down is started again, with no restart record, once the replacement passes. It is then probed within its `start_period` before any changed service that depends on it starts. A failed probe there aborts the deploy (ADR-0056 2.4).
- [ ] **Never-run edges:** an edge to a never-run service outside the deployment is ignored and logged.
- [ ] **Aborting** restarts the containers it stopped in dependency order only, ignores edges to services without a pointer, and never waits for health.
- [ ] **Reconciler** (`src/orchestrator/reconcile.rs`), using the last committed deployment's revision:
  - A stopped current container starts once each dependency is running and has passed its check since its last (re)start.
  - A dependency that isn't running holds without a bound and at no crash-loop cost.
  - A running dependency that hasn't passed within min(`start_period`, 5 min) releases the hold, and one reconciler-restart event (ADR-0055) records it.
  - `recover()` starts only containers that are ready; the periodic pass starts the rest as startup starts.
  - The UI state "waiting for <dep>" is exposed.
- [ ] **Monitor** (`monitor.rs`): a (re)start resets the status to `Unknown`. There is a grace of min(`start_period`, 5 min) with no events, and the first pass wakes the reconciler. The monitor reads the last committed deployment's revision, not `latest_revision`.
- [ ] Deployment events name the services starting and what they waited for. The `failed` event lists the dependents that never started. States and transitions in `machine.rs` are unchanged.

## Testing
- Fake-runtime orchestrator tests (`tests.rs`, `zero_downtime_tests.rs`):
  - first deploy of app → db where db is stop-first with no previous generation;
  - a diamond;
  - a held-back app behind a replaced db;
  - an unchanged dependency down → fail;
  - an unchanged dependent exited and restarted;
  - a never-run edge;
  - abort order.
- `reconcile_tests.rs`:
  - a hold with the dependency down;
  - a hold released after `start_period`, with one event;
  - deferred startup starts;
  - no crash-loop budget spent while held.
- `monitor_tests.rs`: grace and reset.
- An e2e stack whose alphabetical order is wrong deploys cleanly.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0056 (Decisions 2, 3); ADR-0017, ADR-0028, ADR-0048, ADR-0055
- `docs/architecture.md` §4 step 5, §5.1, §5.4, §5.5
- `src/orchestrator/engine.rs`, `plan.rs`, `machine.rs`, `reconcile.rs`, `monitor.rs`