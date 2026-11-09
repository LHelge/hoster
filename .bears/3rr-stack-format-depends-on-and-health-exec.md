---
id: "3rr"
title: "Stack format: `depends_on` and `health.exec`"
status: done
priority: P1
created: "2026-11-09T17:22:16.187295405Z"
updated: "2026-11-09T19:57:58.265621657Z"
tags:
  - stack
depends_on:
  - "4qn"
parent: qq9
attempts: 1
---

## Summary
Parse and validate the two new service fields from ADR-0056, and carry them in the validated model. Pure parser work: nothing runs them yet.

## Acceptance Criteria
- [ ] `Service.depends_on: BTreeSet<Name>`. Errors (all collected, with spans and field paths): unknown service, duplicate entry, self-dependency, cycle. A cycle names its members in order, e.g. `app → worker → app`.
- [ ] `Stack` exposes the dependency graph through narrow methods: a service's dependencies, its transitive dependencies, and a deterministic dependency order (name as tie-break). The engine, `Plan` and the reconciler use them; nothing else re-derives the graph. ADR-0056 starts each service once its own dependencies pass, so don't model strict waves.
- [ ] Health validation per ADR-0056 Decision 4: at most one of `http` / `tcp` / `exec`; `exec` is the only probe allowed on a non-routed service; a timings-only `health` table on a non-routed service is an error.
- [ ] `Probe::Exec { command: Vec<String> }`: non-empty, first element non-empty, mutually exclusive with `http` / `tcp`. `health` is now allowed on non-routed services when it uses `exec`; `http` / `tcp` on a non-routed service stays an error.
- [ ] `ConfigHash::new` (`src/orchestrator/plan.rs`) adds the exec probe only when set, so no existing hash changes. `depends_on` is **not** in the hash (ADR-0056 Decision 1): changing it reorders starts and never recreates a container.
- [ ] stack-format.md §2.2 / §2.4 document both fields; the JSON Schema is regenerated if zcu has landed.

## Implementation Notes
- `src/stack/raw.rs` (`RawService`, the health raw types, validation pass); `src/stack/mod.rs` (`Service`, `Probe`); `src/health/mod.rs` matches on `Probe`, so add an `unimplemented`-free placeholder arm that returns a clear `HealthError` until the exec check task lands.
- Keep `Name` as the key type; reuse the existing diagnostic and span helpers.

## Testing
- `src/stack/tests.rs` + insta snapshots: valid chains, diamond, cycle of 2 and 3, unknown name, self-dependency, exec on routed / non-routed, exec + http conflict.
- Dependency-graph unit tests (transitive dependencies, deterministic order).
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0056 (task 4qn); ADR-0038
- `site/content/wiki/stack-format.md` §2.2, §2.4
- `src/stack/raw.rs`, `src/stack/mod.rs`, `src/orchestrator/plan.rs`