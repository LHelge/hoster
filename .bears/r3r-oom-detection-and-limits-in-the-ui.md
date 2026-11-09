---
id: r3r
title: OOM detection and limits in the UI
status: open
priority: P2
created: "2026-11-09T17:23:25.599826110Z"
updated: "2026-11-09T17:23:25.599826110Z"
tags:
  - orchestrator
  - web
  - observability
depends_on:
  - xxb
parent: "9q5"
---

## Summary
Make "killed for exceeding its memory limit" a first-class reason, and show configured limits.

## Acceptance Criteria
- [ ] When the reconciler finds an exited container (`src/orchestrator/reconcile.rs`), it inspects `State.OOMKilled` (via the runtime's inspect) and records the reason with the restart in its restart stats. A crash loop caused by OOM is labelled as such in the monitor (`monitor.rs`) and in `UiEvent::Health`.
- [ ] An OOM exit during a deploy's health check aborts with "OOM-killed (memory limit 512 MiB)" in the deployment's failure reason.
- [ ] The stack overview shows each service's limits and runtime options, plus the last OOM time if any. The dashboard warnings list services OOM-killed in the last 24 h.
- [ ] If epic qmw (resource usage meters) has landed, its memory meter shows usage against the limit; otherwise leave a note on qmw.

## Testing
- Reconciler and monitor tests with the fake runtime reporting `OOMKilled`; engine abort-reason test; web rendering tests.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M13 ADR (task w2w); ADR-0028, ADR-0045
- `src/orchestrator/reconcile.rs`, `src/orchestrator/monitor.rs`, `src/runtime/mod.rs`, `src/web/stacks/overview.rs`, `src/web/dashboard.rs`