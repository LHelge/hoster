---
id: "5ae"
title: "Stats: per-minute traffic deltas with a mergeable, encodable latency histogram"
status: done
priority: P1
created: "2026-11-11T09:08:20.531155347Z"
updated: "2026-11-11T09:33:26.862332517Z"
tags:
  - stats
  - observability
depends_on:
  - mje
parent: t97
attempts: 1
---

## Summary

Give the in-memory `Traffic` counters what the recorder needs: a way to take, per route, what was counted since the last take (status counts, upstream errors, latency histogram), and a latency histogram that can be stored and merged. No store or UI change in this task; the existing snapshot and rolling window keep working until the close-out task removes them.

## Acceptance Criteria

- [x] Each route's counters keep a cumulative latency histogram beside the status counters, recorded lock-free on the proxy's hot path (no new locks; at most a couple of atomic adds more per request)
- [x] `Traffic` has a drain operation returning one delta per route (and one for unrouted) with requests since the previous drain: host, prefix, owner, counts per `StatusClass`, upstream errors, latency histogram. Routes without requests since the last drain return nothing
- [x] The histogram is a public type of `stats` with `Sum`/`AddAssign`, p50/p95 (as `Latency` has today), and a compact versioned byte encoding via `From`/`TryFrom` (sparse: most of the 108 buckets are empty). Decoding unknown versions or malformed bytes is an error, not a panic
- [x] A route that changed owner starts a new series, as its counters do today
- [x] Counters of routes that are gone are dropped once drained and idle, so the registry no longer grows until restart

## Implementation Notes

- `src/stats/traffic.rs` (`RouteCounters`, `Registry`), `src/stats/latency.rs` (`Histogram`, `Bucket`, `Latency`).
- Draining diffs cumulative atomics against the values of the previous drain (kept by the drainer, not on the hot path); do not reset atomics the proxy threads write.
- The bucket layout (`SUB`, `BUCKETS`) becomes a persisted format: say so at the constants and cover it with a test that fails when it changes.
- Behaviour on types (CLAUDE.md): the delta and histogram are types with methods and std traits, no free functions.

## Testing

- Drain twice: the second holds only what came between; concurrent recording loses nothing across a drain (sum of drains equals the total).
- Histogram encode/decode round trip, empty histogram, corrupt input, merged percentiles equal the percentiles of all samples together.

References: ADR-0062, ADR-0045; `src/stats/`; the proxy's `logging` hook in `src/proxy/mod.rs` for the hot path.