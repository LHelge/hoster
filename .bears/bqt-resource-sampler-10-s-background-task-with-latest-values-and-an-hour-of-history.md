---
id: bqt
title: "Resource sampler: 10 s background task with latest values and an hour of history"
status: open
priority: P1
created: "2026-11-12T00:07:55.945321791Z"
updated: "2026-11-12T00:07:55.945321791Z"
tags:
  - observability
  - runtime
depends_on:
  - "3vg"
  - bny
  - "76n"
parent: qmw
---

## Summary

The background sampler of ADR-0066: every 10 s it reads the stats of every running managed container (`ContainerRuntime::stats`, task bny) and the host's CPU, memory and disk (`Machine`, task 76n), and keeps the latest values plus an hour of history in memory for the web UI to read without touching podman.

## What

- A deep module under `src/stats/` (e.g. `src/stats/resources.rs`) with a narrow public API: a cheaply clonable handle (`Arc` inside) that pages read, and a `run(token)` task the supervisor spawns.
- **Per tick:** list managed containers, read stats of the running ones concurrently (at most 8 at a time) with one 5 s deadline for the whole tick, after which an open call counts as unknown (ADR-0066 §2); compute CPU % (of one core) from the cumulative-time delta over the wall time between the two readings, measured on the sampler's monotonic clock when each answer arrived; read host CPU delta, memory and the disk usage of podman's storage root (from `EngineInfo`, read once and retried while unknown). A tick that overruns is skipped (`MissedTickBehavior::Skip`), never queued.
- **State:** per container (keyed by container id, with its stack and service from the labels) the latest reading and a ring of 360 samples, each with its time and optional figures; the same for the host (CPU, memory used/total, disk used/total). A ring is kept while its container is listed (a stopped one gets unknown samples) and dropped once it is no longer listed. The first sample of a container has no CPU value, nor does one whose counter went backwards (a restart gives a new cgroup), nor the host's first tick.
- **Reads:** latest and history for a stack's containers, and for the host; values are unknown (`None`) before the first tick and when podman does not answer. A failing runtime is logged once per state change, not every 10 s, and never fails a page.
- **Change signal:** something a live page can await to learn that a new sample landed (e.g. a `tokio::sync::watch` tick counter), for the SSE task (stack overview) to use.
- **Wiring:** spawned by the supervisor with a child of the root token, in the phase where the other background tasks start; the handle reaches the web app's state. Time is injectable or the tick function is callable directly so tests need no sleeping (see how `src/stats/recorder.rs` is tested).
- `docs/architecture.md` §3.1/§4 and the "Resource usage" section match what was built (the ADR task wrote the first version).

## Acceptance Criteria

- [ ] Tests with `FakeRuntime` and `Machine::under`: CPU % from two ticks, first sample unknown, counter going backwards unknown, ring capped at 360, gone containers dropped, runtime failure keeps the host figures and marks containers unknown, stopped containers skipped
- [ ] The sampler runs in a real `hoster` start (supervisor) and stops on shutdown
- [ ] No store access, no migration
- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

References: ADR-0066, ADR-0045, ADR-0050; `docs/architecture.md` §3, §4; `src/stats/mod.rs`, `src/stats/recorder.rs`, `src/supervisor.rs`, `src/machine.rs`, `src/runtime/mod.rs`.