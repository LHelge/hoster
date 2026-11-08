---
id: cu8
title: "Stack overview: KPI tiles, slowest routes, routes and volumes cards"
status: open
priority: P2
created: "2026-11-08T18:20:03.989743305Z"
updated: "2026-11-08T18:20:03.989743305Z"
tags:
  - web
  - ui
depends_on:
  - "7wg"
parent: "8bf"
---

Add the data-driven sections of `docs/design/StackDetail.dc.html` that hoster can fill today.

- **KPI tiles** for the stack: requests since start, p50 and p95 (latency window), 5xx share since start, restarts across services, deployments live/failed over 30 days. Leave out uptime-30d and egress (no data).
- **Slowest routes**: the stack's routes from the per-route traffic stats, sorted by p95, each with a `Meter` relative to the slowest, plus the counts by status class and upstream errors (this takes over the dashboard's per-route Traffic table for this stack).
- **Routes card**: one row per route, `https://host/prefix → service:port`, with tags for the TLS state (days left from the cert map, or "TLS pending"), `strip prefix` when set, and per-route timeouts when not default. Read-only: routes come from the stack file, so there's no "+ add domain".
- **Volumes card**: the stack's named volumes with their mount targets per service, from the active revision. No sizes (that's the images & storage follow-up).
- Leave out the 24h status chart (traffic-history follow-up).

References: ADR-0007, ADR-0009, ADR-0010, ADR-0011, ADR-0030, ADR-0045, `src/stats/traffic.rs`, `src/stack/mod.rs` (`Route`), `src/web/stacks.rs`, `docs/design/StackDetail.dc.html`.