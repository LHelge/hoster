---
id: "3rr"
title: "Stack format: `depends_on` and `health.exec`"
status: open
priority: P1
created: "2026-11-09T17:22:16.187295405Z"
updated: "2026-11-09T17:22:16.187295405Z"
tags:
  - stack
depends_on:
  - "4qn"
parent: qq9
---

## Summary
Parse and validate the two new service fields from the M11 ADR, and carry them in the validated model. Pure parser work: nothing runs them yet.

## Acceptance Criteria
- [ ] `Service.depends_on: BTreeSet<Name>`. Errors (all collected, with spans and field paths): unknown service, self-dependency, cycle. A cycle names its members in order, e.g. `app → worker → app`.
- [ ] `Stack` exposes the dependency order as a method, e.g. `Stack::start_order()` returning topological waves `Vec<BTreeSet<Name>>`, deterministic by name within a wave. The engine and reconciler use it; nothing else re-derives it.
- [ ] `Probe::Exec { command: Vec<String> }`: non-empty, first element non-empty, mutually exclusive with `http` / `tcp`. `health` is now allowed on non-routed services when it uses `exec`; `http` / `tcp` on a non-routed service stays an error.
- [ ] `ConfigHash::new` (`src/orchestrator/plan.rs`) covers `depends_on` and the exec probe. Check against the ADR whether `depends_on` belongs in the hash; recommend yes for simplicity.
- [ ] stack-format.md §2.2 / §2.4 document both fields; the JSON Schema is regenerated if zcu has landed.

## Implementation Notes
- `src/stack/raw.rs` (`RawService`, the health raw types, validation pass); `src/stack/mod.rs` (`Service`, `Probe`); `src/health/mod.rs` matches on `Probe`, so add an `unimplemented`-free placeholder arm that returns a clear `HealthError` until the exec check task lands.
- Keep `Name` as the key type; reuse the existing diagnostic and span helpers.

## Testing
- `src/stack/tests.rs` + insta snapshots: valid chains, diamond, cycle of 2 and 3, unknown name, self-dependency, exec on routed / non-routed, exec + http conflict.
- Topological-wave unit tests.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M11 ADR (task 4qn); ADR-0038
- `site/content/wiki/stack-format.md` §2.2, §2.4
- `src/stack/raw.rs`, `src/stack/mod.rs`, `src/orchestrator/plan.rs`