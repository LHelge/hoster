---
id: "5q8"
title: "Stack pages: last-hour traffic on the stack cards and the stack overview"
status: done
priority: P2
created: "2026-11-11T09:09:29.444561821Z"
updated: "2026-11-11T11:02:03.797438043Z"
tags:
  - web
  - ui
  - observability
depends_on:
  - h6k
parent: t97
attempts: 1
---

## Summary

The pages without a range picker show the **last hour** from traffic history (owner decision, ADR-0062): the stack cards on `/stacks` (`docs/design/Stacks.dc.html`) and the stack overview (`docs/design/StackDetail.dc.html`). This replaces their "since hoster started" counts and rolling-window percentiles.

## Acceptance Criteria

- [x] Stack card: req/min (last 5 min), p95 and 5xx share over the last hour, and a sparkline of requests per minute over the hour, toned by the stack's condition. A stack that routes nothing keeps showing no traffic row
- [x] Stack overview tiles: req/min with the sub-line "peak N, M min ago" (the busiest minute of the hour, relative time so no time zone is involved), p50, p95, 5xx rate with `n of m`, each labelled "last hour"
- [x] Stack overview "Responses by status · 1h" card: stacked bars, 60 x 1 min, with the legend totals, for this stack's routes
- [x] Any per-route traffic the overview shows today reads from the same hour, or is removed if it only repeated "since start" counters; say which in the commit
- [x] The window is one constant shared by both pages and named in their labels, so changing it later is one edit
- [x] No `style=` attributes; works without JavaScript; no sideways page scroll at 375 px
- [x] Not included: a range picker, a slowest-routes panel, egress, uptime % (proposals or not adopted)

## Implementation Notes

- `src/web/stacks/list.rs` (`StackCard`, `TrafficLine`), `src/web/stacks/overview.rs` (`latency_window` and its template), the templates for both.
- Reuse what the dashboard task built: the history query (range 1h, per stack), the chart components, the delta-free `TrafficLine`.
- The list page reads history for every stack in one query.
- Extend the `ui_preview` fixtures for both pages.

## Edge Cases

- A stack deployed minutes ago: mostly empty bars. A stack with no requests in the hour: zero req/min, `-` percentiles, flat sparkline.

## Testing

- Web tests for both pages: figures from fixture history, the peak line, the empty states.

References: ADR-0062, ADR-0054; `docs/design/Stacks.dc.html`, `docs/design/StackDetail.dc.html`; `src/web/stacks/`.