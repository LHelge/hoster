---
id: "7rd"
title: Range picker on the stack overview
status: proposed
priority: P3
created: "2026-11-11T09:09:41.542251849Z"
updated: "2026-11-11T09:09:47.032174085Z"
tags:
  - design-followup
  - observability
  - web
---

Left out of M16 (t97) by the owner on 2026-11-11; proposed as a follow-up.

M16 gives the dashboard a 1h-30d range picker and fixes the stack pages at the last hour. This would put the same picker on the stack overview, so one stack's tiles and chart can be read over 6h to 30d, and add the p50/p95 line chart there. The history query and charts already take a range; the work is the control, the labels and the peak line (only meaningful on minute-bucket ranges).

References: ADR-0062, ADR-0054; epic t97.