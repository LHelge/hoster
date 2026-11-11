---
id: g8r
title: "UI: server-drawn stacked bars, percentile lines and sparklines"
status: done
priority: P2
created: "2026-11-11T09:08:57.934145636Z"
updated: "2026-11-11T10:22:43.785724295Z"
tags:
  - web
  - ui
depends_on:
  - mje
parent: t97
attempts: 1
---

## Summary

The three chart components the mockups use, drawn as SVG on the server like `Meter` (ADR-0054: no `style=`, no script): a stacked bar chart of responses by status class, a two-line chart (p50, p95) and a sparkline. They take plain numbers, so this task needs no history backend and can be built and looked at in `ui_preview` first.

## Acceptance Criteria

- [x] `StackedBars`: one bar per bucket, segments 2xx/3xx/4xx/5xx bottom to top in the status colours (green, blue, peach, red), other classes (1xx, no response) in a neutral tone; horizontal grid lines, a y-axis scale with rounded maxima, relative x labels (`-24h … now`); a legend with each class's total
- [x] `LineChart`: p95 (yellow) and p50 (teal) polylines over the same x axis, y labels as `Elapsed`; a bucket without samples breaks the line rather than drawing zero
- [x] `Sparkline`: 96 x 28, one polyline, tone by class (teal default, peach degraded, muted stopped), flat line for no data, `aria-hidden`
- [x] Geometry is attributes computed in Rust; colours and strokes come from classes in `assets/hoster.css` using the existing tokens, in Mocha and Latte, non-text contrast at least 3:1 against the card
- [x] Each chart has `role="img"` and an `aria-label` that summarises it in words (range, totals, where the peak was); each bar has a `<title>` with its time and counts as the no-script tooltip
- [x] Charts scale to their card's width (viewBox, `preserveAspectRatio`), 375 px included, without the page scrolling sideways
- [x] An all-zero series renders an empty chart with its axes, not a division by zero or NaN
- [x] A `ui_preview` page or section shows each component with fixture data, an empty series and a single spike

## Implementation Notes

- `src/web/ui.rs` (`Meter`, `Tone`, `impl HtmlSafe`) is the pattern; put the charts in their own module if `ui.rs` grows too large, with a narrow API (CLAUDE.md: deep modules).
- Mockup geometry: `docs/design/Main.dc.html` lines 120-175 (bars 600 x 180 line chart, y gutter, legend), sparkline at line 200 and `Stacks.dc.html` line 160.
- `vector-effect="non-scaling-stroke"` keeps line widths when the SVG stretches.
- Escape every label that reaches an attribute (`Meter`'s test for `<x>` shows how).

## Testing

- Exact SVG output for small series; scaling (the tallest bar reaches the plot height); empty and single-value series; escaping.

References: ADR-0054, ADR-0053; `docs/design/README.md` (tokens); `src/web/ui.rs`, `src/web/ui_preview.rs`, `assets/hoster.css`.