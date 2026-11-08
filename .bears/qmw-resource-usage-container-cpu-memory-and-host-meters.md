---
id: qmw
title: "Resource usage: container CPU/memory and host meters"
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:30.005241476Z"
updated: "2026-11-08T18:22:30.005241476Z"
tags:
  - design-followup
  - observability
  - runtime
  - web
---

Stub from the design pass (epic 8bf). Break down when picked up.

## What

CPU and memory columns in the stack services table (`docs/design/StackDetail.dc.html`), and the **host cpu / mem / disk meters** at the bottom of the sidebar (every design screen).

## Why it isn't in the design pass

It needs new data: podman container stats through bollard (one-shot `stats?stream=false` per container, or a background sampler), and host CPU from `/proc/stat` deltas, memory from `/proc/meminfo`, and disk from `statvfs` on the data dir and the podman storage root.

## Size and shape

Medium. A background sampler that caches the latest values (the sidebar renders on every page and must not call podman each time); server-drawn meters (ADR-0054). Memory is shown without a limit (no resource limits in v1, ADR-0038), unless limits arrive.

## Open points

The sampling interval and cost on a small VPS; whether to keep a short history (that would tie into the traffic-history epic).

References: ADR-0002, ADR-0038, ADR-0045, `src/runtime/`, `src/web/pages.rs`.