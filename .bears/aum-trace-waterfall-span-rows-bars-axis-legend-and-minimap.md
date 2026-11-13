---
id: aum
title: "Trace waterfall: span rows, bars, axis, legend and minimap"
status: done
priority: P2
created: "2026-11-12T19:37:26.941263741Z"
updated: "2026-11-13T22:12:15.543469940Z"
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
The waterfall of the selected trace, server-rendered: a header with duration, spans, depth and errors; a legend of services with span counts; a minimap; and one row per span in DFS order, indented by depth, with a bar on a shared time axis.

## Acceptance Criteria
- [ ] Header: TRACE · stack, root span name, trace id, date and time; DURATION, SPANS, DEPTH, ERRORS (red when non-zero)
- [ ] Legend: one chip per service in the categorical palette with its span count
- [ ] Rows in DFS order by start time; the name column indents by depth (depth classes up to a cap, then the cap); an error badge on spans in error; a tag from the span's attributes where it helps (`db.system` → `pg`, `redis`; `process.command` → `exec`)
- [ ] Bars: SVG `rect` per row on one axis from the trace's start to its end, with five tick labels; width at least a visible minimum; the duration label right of the bar, or inside it when the bar is wide, or left of it near the right edge, as the mockup places it; error spans keep their colour with a red ring
- [ ] Minimap: every span as a 3 px line in a small SVG above the rows
- [ ] Partial traces render: spans whose parent is missing hang under the root, marked; a trace over the span cap shows the first N with "N more not shown"
- [ ] Each row is a link (`?trace=…&span=<id>`) that selects the span for the details task; keyboard reachable
- [ ] No inline styles and no new script (ADR-0054, ADR-0060)

## Implementation Notes
- Bars in SVG need per-row positions; render one `<svg>` per row with a `viewBox` of 0–1000 and `x`/`width` attributes, so the column scales with the page without CSS widths
- Put layout numbers (left, width, label side) on a view type computed in Rust, tested without templates

## Testing
- View-type tests on the mockup's traces `t1` and `t2`: order, depth, bar positions, label sides, partial-trace placement
- Handler test: the selected trace renders below the list
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- `docs/design/Telemetry.dc.html` (waterfall, `renderVals()`), `docs/design/README.md` (Telemetry notes), ADR-0054, ADR-0060
- `src/web/charts.rs`, the trace query task `txt`