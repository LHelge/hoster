---
id: txt
title: "Traces: store queries for the trace list and for one trace"
status: open
priority: P2
created: "2026-11-12T19:36:58.371075490Z"
updated: "2026-11-12T19:37:56.294166195Z"
tags:
  - store
  - observability
depends_on:
  - aqy
parent: qnh
---

## Summary
The read side the Traces tab needs: a list of traces over a range (one row per trace, from its root), and one trace's spans assembled into a tree, with the proxy's two spans built from the request row (ADR-0069 §9) and the counts of log records per trace and span.

## Acceptance Criteria
- [ ] `TraceQuery`: range, scope (one stack or all), services, status (ok / error), minimum duration, sort (newest, slowest, errors first), page; each row has trace id, start, root span name, stack, the set of services, span count, duration, status (error if any span is in error)
- [ ] The total number of traces in the range for the tab's count and the "9 of 18,204" heading
- [ ] A trace with only the proxy's spans is marked "app not instrumented" (one service in the set)
- [ ] `load_trace(trace_id)` returns all its spans (capped by a constant, with the true count when over it), each with its children ordered by start, depth, and whether it has a parent that is missing (late or lost spans: shown under the root, marked)
- [ ] The proxy's two spans are built from the request row (trace id = request id, the row's two span ids and upstream start time), behind the same `Span` type as stored spans, so the UI does not know the difference; a row from before those columns existed gives no spans
- [ ] Log records per trace and per span id (counts, and the records themselves for a span)
- [ ] Root spans for the duration scatter: (start, duration, status) over the range, capped to a fixed number of points sampled evenly when there are more

## Implementation Notes
- Waterfall math belongs on the trace type, not in templates: depth, DFS order, the critical path (recursively the child with the latest end), self time (duration minus the union of child intervals). `renderVals()` in `docs/design/Telemetry.dc.html` has reference implementations
- A trace can have more than one root when spans are lost; keep all, ordered by start

## Testing
- Store tests with seeded spans and request rows: list sorts and filters, proxy-only traces, missing parents, the span cap, critical path and self time on the mockup's sample trace (`t1`)
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0069 (§6, §9), ADR-0067
- `docs/design/Telemetry.dc.html` (`renderVals()`), the span table task `aqy`