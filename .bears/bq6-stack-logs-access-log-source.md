---
id: bq6
title: "Stack logs: access-log source"
status: open
priority: P3
created: "2026-11-12T13:27:00.855409220Z"
updated: "2026-11-12T13:27:00.855409220Z"
tags:
  - web
  - observability
  - proxy
depends_on:
  - cb6
  - z7r
  - qr7
parent: psq
---

## Summary
The **access log** entry of the stack logs tab's source switch: the proxy's stored request rows (ADR-0067 §5, written by task `z7r` of epic `usf`, read through `qr7`) for the stack's routes over the chosen time range. This is where per-path questions are answered that traffic history (per route and minute, ADR-0062) cannot: which paths 404, which requests were slow.

## Acceptance Criteria
- [ ] `?source=access` lists the stack's request rows in time order: time, service, method, path, status (toned by class as the traffic charts are), total duration, response bytes, client address
- [ ] Filters: the service chips; a status filter (all, 4xx, 5xx, a single code via the parameter); grep on the path; a minimum duration (`?slower=500ms`). Time range, paging and follow work as for containers
- [ ] The footer shows requests listed and the counts of 4xx and 5xx
- [ ] A row expands to the rest: host, route prefix, time to the response header, whether the upstream failed, the request id
- [ ] The request id links to the container records that carry it (the field filter of task `qs4`, if it has landed); the link is omitted otherwise
- [ ] Download works for this source, if the download task has landed, with one request per line in a common-log-like text format and in JSON lines
- [ ] The source appears in the switch only when the store has the `requests` table populated by `usf`; before that the tab is unchanged
- [ ] The Logs sentence in `site/content/wiki/overview.md` mentions the access log, and that client addresses and paths are kept for 7 days

## Implementation Notes
- Read through a query on the log store type beside the records query, sharing the range, paging-by-id and scan-cap machinery; add an index on the requests table for `(stack, time)` in `usf` if it is missing
- Durations are formatted as the stack overview formats latencies
- Leave a place for the trace link the waterfall epic (`qnh`) adds; do not build it here

## Edge Cases
- Requests to a route whose stack was the owner at the time but is not now are still this stack's rows (the row stores stack and service, no join to the route table)
- Paths can be long and contain anything: escape, wrap, and cap what is rendered in the collapsed row
- The proxy dropped rows under load: show the drop counter for the range if `usf` exposes it

## Testing
- Handler tests with seeded request rows: filters, status tones, expansion, paging, the source being hidden without data
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§5, §8), ADR-0062, ADR-0054
- `docs/design/StackLogs.dc.html`, `src/web/stacks/logs.rs`, `src/web/charts.rs` (status tones)