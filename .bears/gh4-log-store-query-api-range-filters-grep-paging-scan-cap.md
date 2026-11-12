---
id: gh4
title: "Log store: query API (range, filters, grep, paging, scan cap)"
status: done
priority: P2
created: "2026-11-12T19:33:20.364893266Z"
updated: "2026-11-12T20:57:43.571575927Z"
tags:
  - store
  - observability
depends_on:
  - "7ne"
parent: f8z
attempts: 1
---

## Summary
The read side of the log store that the logs UI (epic `psq`) builds on (ADR-0067 §8): one query type that narrows by time, stack, service, level and stream with the index, matches a substring or regular expression against message and fields in Rust, and returns pages by record id under a cap on rows scanned.

## Acceptance Criteria
- [ ] A `LogQuery` value: time range, scope (one stack, or all stacks with an optional stack, `hoster` included per ADR-0068), services, a level set, streams, text (substring, case-insensitive) or regex, field filters (exact match on a flattened key), and a page cursor (before/after a record id) with a page size
- [ ] Results are ordered by time then id; a page says whether there are older and newer records
- [ ] The scan visits at most a fixed number of rows per request (a constant); when it stops early the result says how far back it got, so the UI can offer "search older"
- [ ] Regular expressions use the `regex` crate with a compiled-size limit; a bad pattern is a typed error, never a panic
- [ ] `oldest_record_time()` for the "data starts at" notice when the size cap has cut history
- [ ] `count_by_level(range, scope, bucket)` returns counts per time bucket and level (no level counted separately) for the Telemetry severity histogram, using the index only (no message scan unless text is set)
- [ ] Queries run on the read pool and never wait for the recorder
- [ ] Flattening of fields to `(dotted key, value)` pairs lives on the record type (the stack logs fields task uses it)

## Implementation Notes
- Narrow in SQL (`WHERE stack = ? AND time BETWEEN ? AND ?` or the `time` index for all stacks), fetch in chunks, filter in Rust until the page is full or the scan cap is hit
- `cargo tree -i regex` first; `cargo add regex` if missing
- The page cursor is an id, not an offset, so paging is stable while records arrive

## Edge Cases
- Many records in the same millisecond: the id tiebreak keeps pages disjoint
- A range entirely before `oldest_record_time()`: an empty page with the notice, not an error
- Level filter with text lines: records without a level match only "all" (the UI says so)

## Testing
- Seeded-store tests: each filter alone and combined, ordering, paging both ways, scan cap and the early-stop signal, regex errors, histogram buckets
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

**Changed while implementing (2026-11-12):** `count_by_level` narrows with the `(stack, time)`
or `(time)` index and reads each row's `level` from the table (no message scan, no Rust
filtering); neither index covers `level`, and adding it would break the `(time, id)` order the
pages read without a sort, so the schema is unchanged. Text matches the message or a flattened
field written as `key=value`. The cursor is a `RecordKey` (time and id, `<time>.<id>`), not a
bare id, because times are podman's stamps and do not follow id order across containers.
`LogStore` gained a read-only pool for all reads (`head()` included).

## References
- ADR-0067 (§4, §8), ADR-0068
- `src/store/`, `docs/design/StackLogs.dc.html`, `docs/design/Telemetry.dc.html` (logs tab)