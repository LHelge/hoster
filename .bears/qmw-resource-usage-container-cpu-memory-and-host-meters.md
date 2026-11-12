---
id: qmw
title: "Resource usage: container CPU/memory and host meters"
type: epic
status: done
priority: P3
created: "2026-11-08T18:22:30.005241476Z"
updated: "2026-11-12T01:37:45.313062022Z"
tags:
  - design-followup
  - observability
  - runtime
  - web
---

From the design pass (epic 8bf). Broken down on 2026-11-12: ADR-0066 (3vg), runtime stats (bny), host readings (76n), the sampler (bqt), the sidebar meters (hzc) and the stack overview columns (rap).

## Decided by the owner (2026-11-12)

- A background sampler every 10 s; pages read its cache only.
- Short in-memory history (about an hour) for sparklines; nothing persisted.
- The disk meter shows the filesystem of podman's storage root.
- Live updates: the stack overview columns over the page's existing SSE stream, the sidebar meters by an HTMX poll of a small fragment.

## What

CPU and memory columns in the stack services table (`docs/design/StackDetail.dc.html`), and the **host cpu / mem / disk meters** at the bottom of the sidebar (every design screen).

## Why it isn't in the design pass

It needs new data: podman container stats through bollard (one-shot `stats?stream=false` per container, or a background sampler), and host CPU from `/proc/stat` deltas, memory from `/proc/meminfo`, and disk from `statvfs` on the data dir and the podman storage root.

## Size and shape

Medium. A background sampler that caches the latest values (the sidebar renders on every page and must not call podman each time); server-drawn meters (ADR-0054). Memory is shown without a limit (no resource limits in v1, ADR-0038), unless limits arrive.

## Open points

None left; see the decisions above. Persisted history would be a later proposal tied to traffic history (ADR-0062).

## Note from r3r (epic 9q5, ADR-0059)

Limits have arrived: `Service.limits` (`memory: Option<ByteSize>`, `pids`, `cpus`) is in every revision, and the stack overview's services card already shows them per service from the committed revision (`ServiceRow.limits` in `src/web/stacks/overview/services.rs`). The memory meter should show usage against `limits.memory` where set (and the CPU column against `limits.cpus`), and stay limit-free where unset. OOM kills are already named in restart events, crash-loop details and deploy aborts (`ExitReason` in `src/orchestrator/exits.rs`); a meter could link to that rather than duplicate it.

References: ADR-0002, ADR-0038, ADR-0045, ADR-0059, `src/runtime/`, `src/web/pages.rs`.