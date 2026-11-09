---
id: kbh
title: "Engine and reconciler: start in dependency order"
status: open
priority: P1
created: "2026-11-09T17:23:25.583018503Z"
updated: "2026-11-09T17:23:25.583018503Z"
tags:
  - orchestrator
depends_on:
  - rxt
parent: qq9
---

## Summary
Use `Stack::start_order()` in the deploy engine and the reconciler, so a dependent never starts before the services it depends on are healthy. The ADR-0048 atomicity stays intact.

## Acceptance Criteria
- [ ] Each wave of `start_new` (`src/orchestrator/engine.rs`) is created and started, then probed, before the next wave. Services within a wave run in parallel. Today the engine starts everything sequentially and then probes all at once. Dependencies that are unchanged in this deployment impose no wait.
- [ ] Per the ADR, stop-first services **without** a previous generation run in the start-first phase. `Plan` (`src/orchestrator/plan.rs`) exposes that classification; the engine doesn't re-derive it.
- [ ] `replace_stop_first` replaces services in dependency order.
- [ ] The reconciler's restarts (`src/orchestrator/reconcile.rs`, startup and periodic passes) start dependencies first. A dependent whose dependency is not running is deferred, not counted as a crash-loop restart, and shown as "waiting for <dep>".
- [ ] Abort, crash recovery (architecture §5.4) and the deployment state machine (`machine.rs`) are unchanged in shape. If a new event is needed per wave, it goes through the pure transition function with tests.
- [ ] Deployment events show the waves ("starting database, cache", "starting app after database, cache are healthy").

## Testing
- `src/orchestrator/tests.rs` / `zero_downtime_tests.rs` with the fake runtime: first deploy of app→db where db is stop-first; a diamond; a failure in wave 2 aborts and removes waves 1–2; an unchanged dependency causes no wait; a dependent's restart is deferred while its dependency is down (`reconcile_tests.rs`).
- e2e: app + postgres + redis with names chosen so alphabetical order is *wrong* deploys cleanly (`src/e2e_tests.rs` if live podman tests cover deploys).
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M11 ADR (task 4qn); ADR-0017, ADR-0028, ADR-0048
- `docs/architecture.md` §5.1, §5.4
- `src/orchestrator/engine.rs`, `src/orchestrator/plan.rs`, `src/orchestrator/machine.rs`, `src/orchestrator/reconcile.rs`