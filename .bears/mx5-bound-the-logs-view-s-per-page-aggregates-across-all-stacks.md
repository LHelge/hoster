---
id: mx5
title: Bound the logs view's per-page aggregates across all stacks
status: proposed
priority: P3
created: "2026-11-13T12:54:49.276884392Z"
updated: "2026-11-13T12:54:51.167837519Z"
tags:
  - observability
  - store
  - performance
---

## Summary
Found while reviewing `ckc` (Telemetry → Logs). The logs view runs `LogStore::services_in` (chips, generations) and `LogStore::count_unleveled` (the hidden-records note) on every page load. Both read every `log_records` row in the range through the `(stack, time)` index. On the stack tab that is one stack's rows. On Telemetry → Logs, with all stacks over the 7d preset, it is O(all rows in the store), unbounded by any scan cap. (`count_by_level` for the histogram, task `hsj`, has the same shape.)

## Options
- A covering index (e.g. `(time, stack, service, generation, level)`) so the GROUP BY / count reads only index pages: still O(rows) but much cheaper.
- Cap the read (e.g. `LIMIT` in a subquery, as `count_records` does) and show "100,000+" / partial chips when hit.
- Keep a small per-(stack, service, generation, hour) rollup table written by the recorder.

Measure with a store of realistic size (the size cap's default) before choosing.

## References
`src/store/logs/query.rs` (`services_in`, `count_unleveled`, `count_records`, `count_by_level`), `src/web/logs.rs`, ADR-0067 §4, §7.