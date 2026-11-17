---
id: pn8
title: "Log store: remove the span reads the trace list no longer uses"
status: open
priority: P3
created: "2026-11-17T20:39:49.449444968Z"
updated: "2026-11-17T20:39:49.449444968Z"
tags:
  - store
  - observability
  - chore
---

## Summary
Found in the epic review of `qnh` on 2026-11-17. Task `aqy` (epic `xv9`) built `LogStore::root_spans` and `RootQuery` for the trace list, but `txt` merges request rows and stored roots in SQL (`TraceQuery::push_traces`), so neither has a production caller; the public `LogStore::trace_spans` is likewise only a wrapper for tests now (`load_trace` reads through `trace_spans_up_to`). The `spans_roots_by_*` indexes are still used and stay. Left out of the `qnh` PR because it deletes work of another, merged epic.

## Acceptance Criteria
- [ ] `LogStore::root_spans`, `RootQuery` and their re-exports and tests are removed, or have a production caller
- [ ] `LogStore::trace_spans` is removed or made test-only, with the recorder and store tests moved to what production reads
- [ ] The unused `TraceQuery::services` filter and `Trace::critical_path()` are either wired into the UI (service chips that filter the list) or removed
- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0069 (§6), `src/store/logs/query/spans.rs`, `src/store/logs/query/traces.rs`, `src/store/logs/trace.rs`