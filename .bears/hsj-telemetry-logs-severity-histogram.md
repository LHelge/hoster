---
id: hsj
title: "Telemetry logs: severity histogram"
status: open
priority: P3
created: "2026-11-12T19:35:08.992860862Z"
updated: "2026-11-12T19:35:08.992860862Z"
tags:
  - web
  - observability
  - charts
depends_on:
  - ckc
parent: psq
---

## Summary
The "Log records by severity" chart of the Telemetry Logs tab: stacked bars per time bucket for debug/info, warn and error over the chosen range, drawn on the server as SVG like the traffic charts (ADR-0062, ADR-0054). It follows the page's stack and filters.

## Acceptance Criteria
- [ ] Fixed bucket plans per range, about 30 bars: 5m → 10 s, 15m → 30 s, 1h → 2 min, 6h → 15 min, 24h → 1 h, 7d → 6 h (constants)
- [ ] Segments debug/info, warn, error in the mockup's colours as CSS classes; records with no level are a fourth, muted segment, and the legend says "no level"
- [ ] Counts come from the log store's `count_by_level` over the index, not by loading records; with a grep or field filter active the chart is hidden or labelled "unfiltered", whichever is cheaper to make correct (decide and document)
- [ ] Each bar is a link that narrows the range to that bucket
- [ ] An accessible name that states the range and the peak error bucket, as the traffic charts do
- [ ] The time axis has the mockup's four labels in the UI's time format

## Implementation Notes
- `src/web/charts.rs` and `src/web/charts/` have the bar plans, axis and SVG helpers of the traffic charts; extend them rather than drawing from scratch
- SVG attributes (`x`, `width`, `height`, `class`) are allowed under the CSP; `style=` is not

## Testing
- Chart tests in the style of the traffic chart tests: bucket plan per range, stacking, empty range, link targets
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- `docs/design/Telemetry.dc.html` (logs tab), ADR-0062, ADR-0054, ADR-0067 (§8)
- `src/web/charts.rs`, `src/web/charts/`