---
id: mt8
title: Images and storage on the System page
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:30.041760937Z"
updated: "2026-11-08T18:22:30.041760937Z"
tags:
  - design-followup
  - runtime
  - web
---

Stub from the design pass (epic 8bf). Break down when picked up.

## What

The "Images & storage" section of `docs/design/System.dc.html`: a storage breakdown bar (volumes / images / logs out of the filesystem), a table of images (ref, used by which stack, size, pulled, digest) with **unused** ones marked, **remove** for an unused image, **prune unused**, and volume sizes on the stack overview's Volumes card.

## Why it isn't in the design pass

It needs new runtime calls (podman image list, `system df`, volume usage) and touches image garbage collection (ADR-0041 keeps the last 3 successful images per service).

## Size and shape

Medium. The listing and sizes are straightforward through bollard. Remove/prune must never take an image kept for rollback by ADR-0041, so it reuses the GC's notion of "kept". `system df` can be slow, so cache it or load it lazily with htmx.

References: ADR-0002, ADR-0030, ADR-0041, `src/runtime/`, `src/web/system.rs`.