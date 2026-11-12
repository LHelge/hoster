---
id: "5d4"
title: "Log store: retention, size cap and incremental vacuum"
status: open
priority: P2
created: "2026-11-12T19:34:02.717558746Z"
updated: "2026-11-12T19:34:02.717558746Z"
tags:
  - store
  - observability
depends_on:
  - r8w
parent: f8z
---

## Summary
Keep `logs.db` within ADR-0067 §7: hourly, delete records and request rows older than 7 days; if the file is still larger than 1 GiB, delete the oldest hour across both tables until it is not; end each pass with an incremental vacuum so the file shrinks.

## Acceptance Criteria
- [ ] Constants, not settings: 7 days, 1 GiB, the hourly interval
- [ ] The sweep runs at startup and hourly, inside the recorder (one writer), in bounded delete batches so a big sweep never holds a long write lock against the commits of the last second
- [ ] Size is measured as `page_count * page_size` minus `freelist_count * page_size` before vacuum, so freed pages that have not yet been returned are not counted twice
- [ ] The size pass removes whole hours, oldest first, from `log_records` and `requests` together, and repeats until under the cap
- [ ] `PRAGMA incremental_vacuum` after each pass gives the freed pages back; the file size on disk falls in a test
- [ ] Neither sweep ever deletes the newest row of `log_records` or `requests`, so ids are never reused (the tables use `INTEGER PRIMARY KEY` without AUTOINCREMENT, as the events table does; see the migration's header)
- [ ] Each pass logs at debug what it removed and how large the file is; the figures are kept for the System page task
- [ ] The span table added later by the OTLP epic plugs into the same sweep (leave a clear place; spans are kept 3 days per OQ-42)

## Edge Cases
- A single hour larger than the cap (a log storm): the size pass deletes that hour too rather than looping forever; at least the current hour is never deleted
- The clock jumped forward: the age sweep may delete more than expected; nothing breaks

## Testing
- Tests with a temporary store and a small cap: age sweep, size sweep across both tables, vacuum shrinks the file, batching
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§2, §7), ADR-0062 (sweeps)
- `src/stats/recorder.rs`