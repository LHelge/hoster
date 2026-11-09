---
id: nbj
title: "Event log store: events table, typed Event, EventLog recorder and 30-day retention"
status: open
priority: P2
created: "2026-11-09T14:04:38.460806483Z"
updated: "2026-11-09T14:04:38.460806483Z"
tags:
  - store
  - observability
parent: "4qj"
---

## Summary
The foundation of ADR-0055: the `events` table, the typed event model, and one `EventLog` handle that producers call to record an event and broadcast it to live pages. No producers yet beyond tests.

## Acceptance Criteria
- [ ] Reversible migration `0008_events` (`sqlx migrate add -r events`): `events (id, at, kind, severity, stack NULL, subject NULL, message, meta NULL)`, STRICT, **no FK on `stack`**, indexes on `(at)` and `(stack, at)`. The `.down.sql` restores the previous schema exactly (store tests enforce it)
- [ ] Typed model: `EventKind` (one variant per ADR-0055 §2 kind) and `Severity` (`Info`/`Success`/`Warning`/`Error`) with `Display`/`FromStr` for the text columns; an `Event` / `NewEvent` pair
- [ ] Store API: record one event (also usable inside an existing transaction), page through events newest-first with a cursor and optional filters (stack, kinds, min severity), delete everything older than a cutoff
- [ ] `EventLog` handle (orchestrator side) that writes through the store, then sends `UiEvent::Event(Event)` on the existing broadcast. A failed write is logged at `warn` and never fails the caller's operation
- [ ] A retention task in the supervisor deletes events older than 30 days once at startup and then daily, cancelled with the rest of teardown

## Implementation Notes
- `src/store/` (new `events.rs` next to `images.rs`, `webhook.rs`), `src/store/types.rs` for the types if they're shared with the web layer
- `UiEvent` lives in `src/orchestrator/mod.rs`; add the variant there. Existing SSE consumers that match on `UiEvent` must ignore or handle the new variant
- Spawn the retention task in `src/supervisor.rs` next to the other `tokio::spawn`s (~line 700)
- Keep the public API narrow: producers see `EventLog::record(NewEvent)`, the web layer sees a query method

## Edge Cases
- Messages must never contain secret values (secret events carry the name only)
- Clock: `at` uses the same unix-seconds convention as the other tables

## Testing
- Store tests: migration up/down round-trip, insert/query/filter/paging, retention cutoff
- `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0055, ADR-0022, ADR-0052 (reversible migrations)
- `src/store/`, `src/orchestrator/mod.rs` (`UiEvent`), `src/supervisor.rs`