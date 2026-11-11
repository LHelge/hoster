---
id: hxc
title: Slowest routes panel on the stack overview
status: proposed
priority: P3
created: "2026-11-11T09:09:41.525596039Z"
updated: "2026-11-11T09:09:47.013798735Z"
tags:
  - design-followup
  - observability
  - web
---

Left out of M16 (t97) by the owner on 2026-11-11; proposed as a follow-up.

The stack overview in `docs/design/StackDetail.dc.html` has a "Slowest routes · p95" card: routes ranked by p95 with a bar each. The mockup ranks URL paths inside the app, which hoster does not see as such; hoster can rank its own routes (host + prefix), which M16's per-route history already holds. Only useful for stacks with more than one route.

Needs: a per-route total in the history query, and a small ranked-bars component.

References: ADR-0062, ADR-0054; `docs/design/StackDetail.dc.html`; epic t97.