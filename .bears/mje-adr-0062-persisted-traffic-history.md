---
id: mje
title: "ADR-0062: persisted traffic history"
status: done
priority: P1
created: "2026-11-11T09:07:59.471694230Z"
updated: "2026-11-11T09:18:35.958194835Z"
tags:
  - docs
  - adr
  - observability
parent: t97
attempts: 1
---

## Summary

Write ADR-0062 recording the owner's decisions for traffic history (the epic body lists them, 2026-11-11) and the design that follows from them, so the implementation tasks have one reference. It amends ADR-0045 ("statistics reset when hoster restarts") and fills the "traffic history, request rates, sparklines" gap ADR-0054 left. The owner reviews it: finish with `review_task`.

## The ADR must state

Decided by the owner:

- Main SQLite database; 1-minute buckets kept 24 h and 1-hour buckets kept 60 d; keyed per route (host, prefix) with owning stack and service.
- The dashboard follows a range picker (1h, 6h, 24h, 7d, 30d); pages without one show a fixed last hour.
- req/min is the rate over the last 5 minutes; the stack overview adds the peak minute of its hour.
- Pages read flushed data only (up to a minute behind); one write per minute; a crash loses at most a minute.
- Out of scope: slowest routes, a stack-page picker, egress bytes (proposals); SLO lines and uptime % not adopted.

Design to propose in the ADR (the owner confirms or changes these in review; none is settled yet):

- **Schema:** two tables of the same shape (minutes, hours): bucket start, host, prefix, stack, service, one count per `StatusClass`, upstream errors, latency histogram. No foreign key to stacks, so a removed stack's history stays until retention (as events do, ADR-0055). How hoster's own routes and unrouted requests are keyed.
- **Only buckets with requests are written.** Buckets are aligned to UTC minutes and hours; a request counts in the bucket it finished in.
- **Latency per bucket** is the existing log-linear histogram (`src/stats/latency.rs`, 108 buckets), stored as a compact versioned blob. Its bucket layout becomes a persisted format.
- **Writing:** a recorder task drains the in-memory counters each minute and, in one transaction, inserts the minute rows and adds them to the hour rows. Flush on graceful shutdown. Retention sweep for both tables.
- **Bars per range:** 1h = 60 x 1 min, 6h = 72 x 5 min, 24h = 24 x 1 h, 7d = 56 x 3 h, 30d = 30 x 1 d (rolling windows from now, so no time zone is involved; axis labels are relative, as in the mockups).
- **Deltas:** when a "vs previous" delta is shown and when it is left out (no data in the previous period).
- **What the dashboard totals cover:** as today, every stack's routes plus unrouted, without the admin UI.
- **Live values that stay in memory:** the in-flight gauge.
- **UI under the CSP:** the picker is `?range=` links, charts are server-drawn SVG styled by class, the live overview stream carries the range.
- Size estimate (rows and bytes per active route) and the write cost.

Alternatives to record: a separate SQLite file, a ring file, 1 min/24 h + 1 h/30 d, one 5-minute table, per-stack and per-service keys, a fixed 24 h window, mean req/min, including the live minute, a metrics endpoint with an external TSDB.

## Acceptance Criteria

- [x] `docs/adr/0062-*.md` in the ADR template, Status: Accepted once the owner has reviewed it
- [x] One row in `docs/adr/README.md`; ADR-0045's and ADR-0054's Status lines and index rows mention 0062
- [x] `docs/architecture.md` gets a short "Traffic history" section (after §11 Event log) and the recorder appears in §3 (tasks) and §4 (startup and shutdown order)
- [x] The later tasks' bodies are corrected if the review changes the design

References: ADR-0045, ADR-0054, ADR-0055, ADR-0022; `docs/architecture.md` §3, §4, §11; `src/stats/`.