---
id: "3vg"
title: "ADR-0066: resource usage sampler, container CPU/memory and host meters"
status: open
priority: P1
created: "2026-11-12T00:07:43.261713411Z"
updated: "2026-11-12T00:07:43.261713411Z"
tags:
  - docs
  - adr
  - observability
parent: qmw
---

## Summary

Write ADR-0066 recording the owner's decisions for resource usage (2026-11-12) and the design that follows from them, so the implementation tasks have one reference. It amends ADR-0045 (observability in the UI) and ADR-0054 (the sidebar host meters and the services table's CPU/memory columns the design pass left out). The owner reviews it in the epic's PR.

## The ADR must state

Decided by the owner (2026-11-12):

- **A background sampler every 10 s** reads container stats from podman and the host's figures; pages only read its cache (the sidebar renders on every page and must never call podman).
- **Short in-memory history**: about an hour of samples kept in memory for sparklines next to the meters; lost on restart; nothing persisted, no migration.
- **The disk meter shows the filesystem that holds podman's storage root** (images, volumes). The System page may also show the data dir's filesystem when it is a different one.
- **Live updates**: the stack overview's CPU/memory columns ride the page's existing SSE stream; the sidebar meters refresh by an HTMX poll of a small fragment at the sampling interval.

Design to propose in the ADR (the owner confirms or changes these in review):

- **Container stats source:** one-shot podman stats through bollard (ADR-0002) per running managed container, vs reading the cgroup files directly. State what podman reports for memory (whether page cache is included) and choose what the memory figure means.
- **CPU %:** the delta of cumulative CPU time between two samples over wall time. Basis: percent of one core (`150%` = 1.5 cores, as `podman stats`/`top`), shown against `limits.cpus` where set (ADR-0059); host CPU is percent of all cores from `/proc/stat` deltas (what counts as busy: everything but idle and iowait).
- **Host memory:** `MemTotal - MemAvailable` from `/proc/meminfo`.
- **Disk:** `statvfs` on podman's storage root (where that path comes from: the engine's info), used = total - available to an unprivileged user.
- **History shape:** a fixed ring of 360 samples (1 h at 10 s) per container and for the host; a container's ring is dropped when the container is gone; memory cost estimate.
- **The first sample** has no CPU delta: shown as unknown (`—`), as is everything when podman does not answer; the sampler never fails a page.
- **Cost:** podman calls per tick on a small VPS; calls for several containers run concurrently with a timeout shorter than the interval; a tick that overruns is skipped, not queued.
- **Tones:** when a meter turns warn/bad (e.g. ≥ 80 % / ≥ 95 % of memory limit or disk).
- **UI under the CSP (ADR-0054):** server-drawn `Meter` and `Sparkline` SVG (`src/web/ui.rs`, `src/web/charts.rs`), no inline styles.
- Interval and history length are constants first (no setting), as ADR-0065 did.

Alternatives to record: sampling on page load, sampling only while watched, 5 s / 30 s, latest only, persisted minute buckets like ADR-0062, streaming `stats?stream=true`, cgroup files directly, the `sysinfo` crate, fullest-of-two or two disk meters, on-load-only figures, a metrics endpoint (ADR-0045).

## Acceptance Criteria

- [ ] `docs/adr/0066-*.md` in the ADR template, Status: Accepted, Date 2026-11-12
- [ ] One row in `docs/adr/README.md`; ADR-0045's and ADR-0054's Status lines and index rows mention 0066
- [ ] `docs/architecture.md`: the sampler appears in §3.1 (tasks on the orchestrator runtime) and §4 (startup and shutdown order), and a short "Resource usage" section follows §12
- [ ] The later tasks' bodies in this epic are corrected if writing the ADR changes the design

References: ADR-0002, ADR-0038, ADR-0045, ADR-0054, ADR-0059, ADR-0062, ADR-0065; `docs/architecture.md` §3, §4, §12; `src/machine.rs`, `src/runtime/mod.rs`, `src/web/ui.rs`.