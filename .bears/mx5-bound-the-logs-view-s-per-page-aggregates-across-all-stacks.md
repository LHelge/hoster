---
id: mx5
title: Bound the logs view's per-page aggregates across all stacks
status: proposed
priority: P3
created: "2026-11-13T12:54:49.276884392Z"
updated: "2026-11-13T13:44:02.249229135Z"
tags:
  - observability
  - store
  - performance
---

## Summary
Found while reviewing `ckc` (Telemetry → Logs) and `bq6` (access log). The logs view runs per-page aggregates on every page load that read every row in the range:
- `LogStore::services_in` (chips, generations) and `LogStore::count_unleveled` (the hidden-records note) on `log_records`, through the `(stack, time)` index. On the stack tab that is one stack's rows. On Telemetry → Logs, with all stacks over the 7d preset, it is O(all rows in the store), with no scan cap.
- `LogStore::request_counts` (from `qr7`), which gives the access-log source's footer counts, counts the whole range with no cap (up to 7d).
- `count_by_level` for the histogram (task `hsj`) has the same shape.

## Options
- A covering index (e.g. `(time, stack, service, generation, level)`) so the GROUP BY / count reads only index pages: still O(rows) but much cheaper.
- Cap the read (e.g. `LIMIT` in a subquery, as `count_records` does) and show "100,000+" / partial chips when hit.
- Keep a small per-(stack, service, generation, hour) rollup table written by the recorder.

Measure with a store of realistic size (the size cap's default) before choosing.

## References
`src/store/logs/query.rs` (`services_in`, `count_unleveled`, `count_records`, `count_by_level`), `src/store/logs/query/requests.rs` (`request_counts`), `src/web/logs.rs`, ADR-0067 §4, §7.