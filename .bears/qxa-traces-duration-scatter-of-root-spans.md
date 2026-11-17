---
id: qxa
title: "Traces: duration scatter of root spans"
status: done
priority: P3
created: "2026-11-12T19:37:26.931069730Z"
updated: "2026-11-17T19:22:12.932660335Z"
tags:
  - web
  - observability
  - charts
depends_on:
  - zyf
parent: qnh
attempts: 1
---

## Summary
The "Trace duration" chart above the trace list: one dot per root span over the range on a log-scale duration axis, errors red, the listed traces highlighted and clickable, drawn on the server as SVG.

## Acceptance Criteria
- [x] Log-scale y axis with the mockup's labels (1 ms to the slowest, at least 1 s); x is time over the range with four labels
- [x] Background dots for root spans in the range (capped and evenly sampled by the store), muted for ok and red for error; the listed traces drawn on top in accent with an `<a>` to `?trace=<id>` and an accessible name ("Open trace POST /api/x, 1.91 s"); the selected one larger
- [x] Legend ok / error / listed below
- [x] An accessible name for the chart stating range, count and the slowest error trace
- [x] Clicking a background area narrows nothing (no brushing); the duration filter of the list is the way to narrow

## Implementation Notes
- Reuse the axis and SVG helpers of `src/web/charts.rs`; the scale is new (log10), put it beside the existing linear scale
- Presentation attributes only (`cx`, `cy`, `r`, `class`); no `style=`

## Testing
- Chart tests: log scale positions, sampling cap, links and labels
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- `docs/design/Telemetry.dc.html` (scatter), ADR-0054, ADR-0062
- `src/web/charts.rs`