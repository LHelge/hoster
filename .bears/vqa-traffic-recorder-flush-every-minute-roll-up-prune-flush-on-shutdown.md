---
id: vqa
title: "Traffic recorder: flush every minute, roll up, prune, flush on shutdown"
status: done
priority: P1
created: "2026-11-11T09:08:57.899829676Z"
updated: "2026-11-11T09:56:32.570380421Z"
tags:
  - stats
  - supervisor
  - observability
depends_on:
  - "5ae"
  - vmm
parent: t97
attempts: 1
---

## Summary

The background task that turns the in-memory counters into history (ADR-0062): every minute it drains `Traffic`, writes the minute rows and their hour rollup, and it prunes both tables to their retention. From this task on, traffic history survives a restart.

## Acceptance Criteria

- [x] A recorder type in `src/stats/` with a `run(cancel)` loop, spawned by the supervisor beside the event-log retention task
- [x] Ticks are aligned to wall-clock minutes; each flush writes what was drained under the minute that just ended. A late or skipped tick still writes one bucket, never a duplicate
- [x] Retention as constants on the type: minute rows 24 h, hour rows 60 d; swept at startup and then periodically
- [x] On graceful shutdown the recorder flushes once more after the proxy has stopped taking requests, so a restart or self-update loses nothing; check the order in `docs/architecture.md` §4 and update it
- [x] A failed write is logged at `warn` and the drained minute is retried with the next flush or dropped, as ADR-0062 says; it never blocks the proxy or grows without bound
- [x] Nothing is written for a minute without requests

## Implementation Notes

- Pattern: `EventLog::run_retention` in `src/orchestrator/event_log.rs` and its spawn in `src/supervisor.rs` (around the retention and image-GC tasks).
- Take the clock as a parameter or trait so tests do not sleep for minutes (`tokio::time::pause` works with `tokio::time::interval`; the wall-clock bucket start needs injecting).
- A request counts in the minute it finished (ADR-0062).

## Testing

- With a paused clock: requests recorded, one tick, rows present with the right bucket start; two ticks give two buckets; an idle minute gives none.
- Cancel flushes the partial minute. A store error does not end the loop.
- Rows older than the retention are gone after the sweep.

References: ADR-0062; `docs/architecture.md` §3 and §4; `src/supervisor.rs`, `src/orchestrator/event_log.rs`.