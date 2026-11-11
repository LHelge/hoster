---
id: t97
title: "M16: Traffic history and charts"
type: epic
status: done
priority: P2
created: "2026-11-08T18:22:29.983359533Z"
updated: "2026-11-11T11:22:20.733132116Z"
tags:
  - design-followup
  - observability
  - web
---

From the design pass (epic 8bf), planned with the owner on 2026-11-11.

## Scope

The design's time-based views, on persisted traffic history: the dashboard **time-range picker** (1h, 6h, 24h, 7d, 30d) with **responses by status** as stacked bars, **p50/p95 lines** and "vs previous period" deltas on the KPI tiles; **req/min** and **sparklines** in the dashboard's stacks table and on the stack cards; the stack overview's traffic tiles and responses chart. See `docs/design/Main.dc.html`, `Stacks.dc.html` and `StackDetail.dc.html` (reference only, ADR-0054).

Today's statistics are counters since hoster started plus a rolling latency window of a few minutes (ADR-0045). This epic persists them as time series and draws them as server-side SVG (ADR-0054, CSP unchanged).

## Decisions (owner, 2026-11-11; recorded in ADR-0062 by the first task)

1. **Storage:** new tables in the main SQLite database, reversible migrations. Not a separate file or a ring file.
2. **Resolution and retention:** 1-minute buckets kept 24 h, 1-hour buckets kept 60 days, so every range (30d included) has a previous period.
3. **Key:** per route (host, prefix) with its owning stack and service. Stack and dashboard figures sum routes; latency histograms merge.
4. **Windows:** the dashboard follows its range picker. Pages without a picker (stack cards on `/stacks`, the stack overview) show a **fixed last hour**.
5. **req/min** is the current rate over the last 5 minutes, independent of the range. The stack overview's tile adds the peak minute of its window.
6. **Freshness:** pages read **flushed data only**, so figures are up to a minute behind. Written once a minute; a crash loses at most the last minute.
7. **In:** peak req/min. **Out** (filed as proposals): a slowest-routes panel, a range picker on the stack pages, egress bytes. Not adopted at all: SLO lines, uptime %.

## Acceptance Criteria

- [x] ADR-0062 accepted; ADR-0045 and ADR-0054 point at it
- [x] Traffic history survives a hoster restart and is pruned to its retention
- [x] Dashboard: range picker, range-based tiles with deltas, responses-by-status and p50/p95 charts, req/min and sparklines in the stacks table
- [x] Stack cards and stack overview show last-hour figures, req/min and a sparkline or chart
- [x] No `style=` attributes or inline scripts; the pages work without JavaScript
- [x] "Since hoster started" figures and the rolling latency window are gone from the UI and the code

References: ADR-0045, ADR-0054, ADR-0055 (the event log, the closest existing pattern), `src/stats/`.