---
id: fsw
title: "Log store: record hoster's own events at info and above (ADR-0068)"
status: open
priority: P2
created: "2026-11-12T19:34:02.727719987Z"
updated: "2026-11-12T19:34:02.727719987Z"
tags:
  - observability
  - logging
depends_on:
  - r8w
  - azv
parent: f8z
---

## Summary
Add the second `tracing` layer of ADR-0068: every event hoster emits at `info` or above becomes a record with service `hoster` in the log store, beside the containers' records, without touching what goes to stderr and the journal.

## Acceptance Criteria
- [ ] `Logging` (src/logging.rs) installs a layer with its own fixed `info` filter, independent of `RUST_LOG`; the stderr formatter and its filter are unchanged
- [ ] Excluded: the log store's own targets (follower, recorder, housekeeping) and `ACCESS_TARGET`. A test shows that a recorder error does not produce a record
- [ ] Record: service `hoster`; stack and generation from the event's own fields or the nearest enclosing span that has them (`LogSpan` in deploy code); stream `hoster`; level from the event; message; fields shaped as the JSON formatter writes them, so the line parser's mapping gives the same result as parsing hoster's own JSON output
- [ ] The layer only calls the recorder handle's `try_record`; with no recorder yet (startup) or after it stopped, events are skipped
- [ ] The per-source line-rate cap of the follower task applies to `hoster` as one source, with the same marker record
- [ ] The module doc of `src/logging.rs` describes the layer

## Implementation Notes
- A `tracing_subscriber::Layer` with `on_event`; span fields are captured in `on_new_span` into extensions (as the JSON formatter does), so the lookup on each event is cheap
- The recorder handle is installed after `Logging::install` runs (logging comes up first); use a `OnceLock` or an `ArcSwapOption` the layer checks
- Reuse the parser's types for level and fields rather than a second mapping

## Edge Cases
- An event inside a span of another stack than its own `stack` field: the event's field wins
- Events from dependencies (`pingora`, `sqlx`, `hyper`) at info: they are recorded too; if any turns out noisy, add it to the exclusions with a comment, not a lower filter

## Testing
- Unit tests with a test recorder: filter level, exclusions, stack from span and from field, no record before the handle is installed
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0068, ADR-0067 (§3, §6, §7), ADR-0044, ADR-0045
- `src/logging.rs` (`LogSpan`, `ACCESS_TARGET`, `DEPLOY_TARGET`)