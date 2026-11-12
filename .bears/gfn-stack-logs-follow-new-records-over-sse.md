---
id: gfn
title: "Stack logs: follow new records over SSE"
status: open
priority: P2
created: "2026-11-12T13:26:20.988302866Z"
updated: "2026-11-12T13:26:20.988302866Z"
tags:
  - web
  - observability
depends_on:
  - yn7
parent: psq
---

## Summary
The **follow** switch of the stack logs tab streams new records of the stack into the page as the recorder commits them (about once a second, ADR-0067 §6), for all services at once and with the tab's filters applied.

## Acceptance Criteria
- [ ] With `follow=on` and the newest page shown, the tab opens an SSE stream (HTMX SSE extension, as the container log page does) that appends records newer than the last one rendered, by record id, with no duplicates and no gaps
- [ ] The stream applies the same service, level and grep filters as the page (shared query type); it works unfiltered if the filter task has not landed
- [ ] Follow is on for a first visit, as today, and off when the user pages back into history; the "newer" link at the newest page turns it back on
- [ ] The stream ends when the browser disconnects, and the server holds nothing for it afterwards
- [ ] The page keeps at most a fixed number of rows in the DOM while following (oldest removed), so a tab left open does not grow without bound
- [ ] A burst larger than one chunk's cap is delivered in order over several events, not dropped
- [ ] If the store becomes unreadable the stream sends one error row and closes

## Implementation Notes
- Prefer a notification from the recorder (a `tokio::sync::watch` of the last committed id, or the existing `UiEvent` broadcast if it fits) over each stream polling the database on a timer; one query per commit per open stream is the budget
- `LogEvents` and `LogChunk` in `src/web/containers.rs` show the SSE shape and the chunking used today; reuse the row template of the tab so followed rows look like rendered ones
- Trimming the DOM needs no script beyond what HTMX offers (`hx-swap` with a `beforeend` target and an out-of-band delete, or CSS on a capped list); the CSP allows no inline script (ADR-0054, ADR-0060)

## Edge Cases
- The stack is removed while a stream is open: the stream closes
- hoster shuts down: streams close without holding the graceful shutdown open
- Clock steps do not matter: the cursor is the record id, not a time

## Testing
- A stream test that commits records in the store and reads the events (see the tests around `log_stream` and `src/web/events/live.rs`): order, no duplicates across the page/stream boundary, filters, disconnect
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§6, §10), ADR-0044, ADR-0021, ADR-0054
- `src/web/containers.rs` (`log_stream`, `LogEvents`), `src/web/events/live.rs`, `src/web/stacks/logs.rs`