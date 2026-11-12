---
id: uw9
title: "Log store: System page figures, doctor check and drop counters"
status: done
priority: P3
created: "2026-11-12T19:34:19.247492295Z"
updated: "2026-11-12T22:17:23.353939381Z"
tags:
  - observability
  - web
  - doctor
depends_on:
  - "5d4"
  - "7n3"
  - fsw
parent: f8z
attempts: 1
---

## Summary
Make the log store's health visible: its size and how far back it reaches on the System page, what was lost and why, and a `doctor` check.

## Acceptance Criteria
- [ ] The System page has a "Log store" row or card: file size against the 1 GiB cap (meter with the tones of ADR-0066), oldest record time, records and request rows in the last hour, and whether the size cap rather than the 7 days now decides the oldest record
- [ ] Losses since hoster started, by cause: recorder channel full, failed commits, line-rate cap (per container, top few), hoster's own events dropped. Zero causes are not shown
- [ ] If `logs.db` could not be opened, the card says so with the error, and the rest of the System page works
- [ ] `hoster doctor` gains a `log_store` check: the file exists with mode 0600 and the expected `auto_vacuum`, opens, migrations match the binary; advisory when the store is off
- [ ] The module docs and `docs/architecture.md` (the section on the state directory) mention `logs.db`

## Implementation Notes
- `src/web/system.rs` and its templates hold the System page; reuse the meter component the host card uses
- Counters come from the recorder and the follower registry (in-memory, reset at start); do not persist them
- `src/doctor/` has the check pattern

## Testing
- Handler tests for the card with a seeded store, with the store off, and with counters set
- A doctor test for each failure of the check
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

**Changed while implementing (2026-11-12):** `Doctor::diagnose` became async, since the
`log_store` check opens `logs.db` with sqlx. A missing `logs.db` is skipped before the first
`hoster run` (no `hoster.db` either) and warns once `hoster.db` exists. The card also shows how
many containers are followed, and names rate-capped containers through the runtime (short id
when unknown); the top three are named, the rest summed.

## References
- ADR-0067 (§2, §7), ADR-0066 (meters), ADR-0054
- `src/web/system.rs`, `src/doctor/`, `docs/architecture.md`