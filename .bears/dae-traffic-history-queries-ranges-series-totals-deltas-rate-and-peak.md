---
id: dae
title: "Traffic history queries: ranges, series, totals, deltas, rate and peak"
status: done
priority: P1
created: "2026-11-11T09:08:57.917843282Z"
updated: "2026-11-11T10:07:44.047896920Z"
tags:
  - stats
  - observability
depends_on:
  - vmm
parent: t97
attempts: 1
---

## Summary

The read side of traffic history, behind the `Stats` handle: one query gives a page everything it draws for a range and a scope. Pages never see rows or SQL. Reads flushed data only (ADR-0062).

## Acceptance Criteria

- [x] A `Range` type (1h, 6h, 24h, 7d, 30d) with `FromStr`/`Display` matching the `?range=` values, a default, and its bar plan from ADR-0062: 60 x 1 min, 72 x 5 min, 24 x 1 h, 56 x 3 h, 30 x 1 d. It picks the minute or hour table itself
- [x] A history query on `Stats` for a range, returning per scope (all hosted traffic, and each stack):
  - a series with exactly the plan's number of bars, oldest first, empty bars included: counts per status class and p50/p95 per bar
  - totals over the range: requests, counts per class, 5xx and 4xx shares, p50, p95
  - the same totals for the previous period of equal length, or none when it holds no data
  - the current rate: requests per minute over the last 5 flushed minutes
  - the peak minute of the range and when it was, for ranges drawn from minute buckets
- [x] "All hosted traffic" is every stack's routes plus unrouted, without hoster's own routes, as the dashboard counts today; hoster's own routes, unrouted and removed stacks remain available for the note under the dashboard's stacks table
- [x] One or two store reads per page render, not one per stack
- [x] `StatsSource` (or the test fake of it) can supply history, so web tests and `ui_preview` get fixture series without a recorder

## Implementation Notes

- `src/stats/mod.rs` (`Stats`, `StatsSource`, `HostStats`) and a new submodule; the histogram merge from the stats task gives percentiles for a bar or a total.
- Bars are rolling windows ending at the newest complete minute (or the current hour for hour-table ranges), not calendar days: no time zone.
- Deltas are for the pages to format; return both periods' totals rather than a percentage.

## Edge Cases

- No history at all (fresh install): every series is empty bars, totals zero, percentiles none.
- A range reaching past retention, or hoster down for part of it: empty bars, not an error.
- Buckets from a stack since removed: counted in "all", attributed to no stack row.

## Testing

- Fixture rows in a test store: bar boundaries for each range, the previous period, rate and peak, the reserved-route exclusion, a merged p95 across routes.

References: ADR-0062; `src/stats/`, `src/web/dashboard.rs` (`TrafficLine`, `OtherTraffic`) for what the pages need.