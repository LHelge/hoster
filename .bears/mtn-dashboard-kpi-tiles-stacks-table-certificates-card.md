---
id: mtn
title: "Dashboard: KPI tiles, stacks table, certificates card"
status: done
priority: P2
created: "2026-11-08T18:19:36.352554835Z"
updated: "2026-11-08T23:57:31.560971640Z"
tags:
  - web
  - ui
depends_on:
  - xhh
  - cu8
  - yz5
parent: "8bf"
attempts: 1
---

Redesign `/` after `docs/design/Main.dc.html`, **using only data hoster has** (ADR-0045). Leave out the time-range picker, the 24h status and latency charts, sparklines, req/min, "vs prev" deltas and the recent-events feed. Those are the `design-followup` epics. Don't draw placeholders for them.

- **KPI tiles**: requests since hoster started; 5xx rate since start; p50 and p95 over the latency window (label the window); stacks `healthy / total` with "n degraded · n deploying"; certificates `valid / total` with "n failing · n expiring". The tile tone follows the worst state.
- **Warnings** (the live SSE overview) becomes an alert list styled like the Certificates alert, shown only when there are warnings.
- **Stacks table** (keeps the live SSE update of today's overview): stack name (mono link), status pill (running / degraded / deploying / failed, derived from health, crash loop and active deployment), `x/y` healthy services, p50/p95 aggregated over the stack's routes, 4xx/5xx share since start, restarts, and the last deployment (#id, state pill, relative time, link to its history). Service-level detail (digest, per-host cert) moves to the stack page. Make sure it is shown there before removing it here.
- **Certificates card**: soonest-expiring first, a days-left meter (out of 90), a status label, "+ n more valid", and a link to `/certificates`.
- Today's per-route **Traffic** table moves to the stack overview ("slowest routes" task) and the unrouted row stays here as a small note. The **Deployments** stats (live/failed/skipped/mean/longest over 30 days) become columns or a tooltip in the stacks table. **Restarts** go into the stacks table and the stack services table. Nothing gets dropped.

References: ADR-0045, ADR-0054, `templates/dashboard.html`, `templates/dashboard_overview.html`, `src/web/dashboard.rs`, `src/stats/mod.rs`, `docs/design/Main.dc.html`.