---
id: qr7
title: "Edge request log: query API on the requests table"
status: open
priority: P2
created: "2026-11-12T19:34:37.130754340Z"
updated: "2026-11-12T19:34:37.130754340Z"
tags:
  - store
  - observability
depends_on:
  - gh4
parent: usf
---

## Summary
The read side for request rows, beside the records query of the log store and sharing its range, paging-by-id and scan-cap machinery. The stack logs tab's access-log source (`bq6`) reads through it.

## Acceptance Criteria
- [ ] A `RequestQuery`: time range, scope (one stack or all), services, status class or single code, minimum total duration, path substring or regex, request id, page cursor and size
- [ ] Ordered by time then id, with older/newer flags, the same scan cap and "stopped early" signal as `LogQuery`
- [ ] Counts for the listed range: rows, 4xx, 5xx (for the footer of the access-log source)
- [ ] Lookup by request id returns the row (for links from container records that logged it)
- [ ] The query and record queries share code for range narrowing, paging and the scan cap rather than duplicating it

## Implementation Notes
- Put both query types behind one small module so the UI sees `LogStore::records(&LogQuery)` and `LogStore::requests(&RequestQuery)` and nothing of the SQL

## Testing
- Seeded-store tests: each filter, paging, counts, request-id lookup
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§5, §8)
- `src/store/`, the log store query task (`gh4`)