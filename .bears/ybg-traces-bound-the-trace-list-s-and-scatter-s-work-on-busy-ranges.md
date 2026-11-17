---
id: ybg
title: "Traces: bound the trace list's and scatter's work on busy ranges"
status: proposed
priority: P3
created: "2026-11-17T17:29:24.082202713Z"
updated: "2026-11-17T20:39:49.469174698Z"
tags:
  - store
  - observability
---

## Summary
`LogStore::traces` and `LogStore::trace_points` (task `txt`) collect and sort every root in the range before the page limit or the sampling applies, and the count, the page and the scatter each do it again. Found in the review of `txt` on 2026-11-17: with 1M requests over 24 h (about 12 req/s, every second one with a stored app span) one Traces tab load costs roughly 0.7 s (count) + 0.9 s (page) + 1.8 s (scatter) after the cheap fix in `txt`, growing linearly with traffic. The rest of the log store bounds its reads with `SCAN_CAP` (ADR-0067); these do not. The status filter, the services filter and the errors-first sort additionally look up spans for every root.

Whether and how to bound it is a design call for the owner: a scan cap as in ADR-0067 (with "showing the newest N" in the UI), one shared pass for count, page and scatter, or a rule on the range the tab offers.


The Telemetry page's Logs tab also calls `LogStore::count_traces` for the Traces tab's count over the logs range (up to 7 days), and the scatter's "slowest error" is only the slowest among the drawn points when the range is sampled, because reading every root's error status is the same unbounded work (tasks `zyf`, `qxa`).

## Acceptance Criteria
- [ ] The owner has chosen the bound (or chosen to leave it); if it changes what the tab shows, it is recorded where ADR-0067 records `SCAN_CAP`
- [ ] A Traces tab load on the measured shape stays within the chosen budget
- [ ] The UI says so when a list or scatter is cut by the bound

## References
- ADR-0067 (search scans the indexed range), ADR-0069 (§6)
- `src/store/logs/query/traces.rs`