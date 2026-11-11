---
id: h6k
title: "Dashboard: range picker, range-based tiles with deltas, traffic charts, req/min and sparklines"
status: open
priority: P2
created: "2026-11-11T09:09:10.808513981Z"
updated: "2026-11-11T09:09:10.808513981Z"
tags:
  - web
  - ui
  - observability
depends_on:
  - vqa
  - dae
  - g8r
parent: t97
---

## Summary

Put traffic history on the dashboard as `docs/design/Main.dc.html` draws it: a time-range picker in the header, tiles that follow the range and compare with the previous period, the two charts, and req/min with a sparkline in the stacks table. This replaces the dashboard's "since hoster started" figures.

## Acceptance Criteria

- [ ] A range picker (1h, 6h, 24h, 7d, 30d) in the page header: a group of links setting `?range=`, the current one marked with `aria-current`; default 24h; an unknown value falls back to the default
- [ ] The live overview stream keeps the range: the SSE URL carries it, so refreshed partials stay on the chosen range
- [ ] Traffic tiles over the range: requests (sub-line `▲ 6.2% vs prev`), 5xx rate (`n of m`), p50 and p95 (sub-line the change vs the previous period). No delta when the previous period has no data. In-flight requests stay on the requests tile, live. Tones as today (5xx rate, p95)
- [ ] "Responses by status" (stacked bars) and "Response time" (p50/p95 lines) cards over the range for all hosted traffic, side by side and wrapping at narrow widths, with the range in their headings
- [ ] Stacks table: req/min (last 5 min), p50, p95, 4xx and 5xx over the range, and a sparkline of requests over the range, toned by the stack's condition; `-` for a stack that routes nothing
- [ ] The note under the table (unrouted, hoster's own routes, removed stacks) counts over the range
- [ ] Tile titles and the module docs say what each figure covers; nothing says "since hoster started" any more
- [ ] Works without JavaScript; no `style=` attributes; no sideways page scroll at 375 px

## Implementation Notes

- `src/web/dashboard.rs` (`Overview`, `kpis`, `StackRow`, `OtherTraffic`, `OverviewEvents`), `templates/dashboard.html`, `templates/dashboard_overview.html`, `src/web/stacks/list.rs` (`TrafficLine`, shared with the stack cards).
- One history query per render (the queries task); the periodic stream refresh (`OverviewEvents::REFRESH`) picks up each minute's flush.
- Delta direction is not good or bad by itself: keep the mockup's neutral tone for deltas.
- Extend the `ui_preview` fixtures with history so the page shows charts, and keep an empty-history variant.

## Edge Cases

- Fresh install or history shorter than the range: charts with empty bars, tiles `-`, no delta.
- A stack removed during the range; a stack created during it.

## Testing

- Web tests in `src/web/dashboard/tests.rs`: each range renders, the picker marks the current one, the SSE URL carries the range, deltas appear and are left out correctly, the admin UI's own requests are not counted.

References: ADR-0062, ADR-0054; `docs/design/Main.dc.html`; `src/web/dashboard.rs`.