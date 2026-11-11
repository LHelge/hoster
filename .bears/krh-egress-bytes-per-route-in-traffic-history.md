---
id: krh
title: Egress bytes per route in traffic history
status: proposed
priority: P3
created: "2026-11-11T09:09:41.560083610Z"
updated: "2026-11-11T09:09:47.047840897Z"
tags:
  - design-followup
  - observability
  - proxy
  - web
---

Left out of M16 (t97) by the owner on 2026-11-11; proposed as a follow-up.

The stack overview mockup has an "EGRESS · 24H" tile. hoster does not count bytes today. It needs the proxy to count response body bytes per route (a new counter on the hot path, streamed bodies and WebSockets included, ADR-0011), a column in both traffic history tables (a reversible migration) and a tile. Decide first whether request bytes (ingress) are counted too, since the migration is the expensive part.

References: ADR-0062, ADR-0011, ADR-0045; `docs/design/StackDetail.dc.html`; epic t97.