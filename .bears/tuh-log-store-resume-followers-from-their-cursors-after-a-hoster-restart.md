---
id: tuh
title: "Log store: resume followers from their cursors after a hoster restart"
status: done
priority: P2
created: "2026-11-12T19:34:02.707030369Z"
updated: "2026-11-12T21:29:50.195165160Z"
tags:
  - observability
  - runtime
depends_on:
  - "7n3"
parent: f8z
attempts: 1
---

## Summary
After hoster restarts (self-update, crash, reboot) it starts a follower for every running managed container, reading from the stored cursor so that lines written while it was down are stored once, up to what podman's size-capped `k8s-file` still holds (ADR-0067 §1).

## Acceptance Criteria
- [ ] At startup, once the reconciler knows the running managed containers, each gets a follower that asks podman for lines `since` its cursor and skips lines at or before it (podman's `since` has second granularity; compare on the nanosecond timestamp)
- [ ] Lines with the same timestamp as the cursor are not duplicated and not lost: the cursor stores the timestamp and the number of lines already taken at that timestamp, or an equivalent rule, with a test
- [ ] A container without a cursor (started while the store was off, or before this version) is read from its start
- [ ] If podman's file has rotated past the cursor, the follower stores one marker record ("lines between X and Y were lost while hoster was down") and continues
- [ ] Containers that exited while hoster was down get their remaining lines read once, then their follower ends
- [ ] Cursors of containers that no longer exist are deleted at startup and by the hourly housekeeping

## Implementation Notes
- The reconciler's startup pass (`src/orchestrator/reconcile.rs`) is the place that already lists managed containers
- Gap detection: compare the first returned timestamp with the cursor; a first line later than the cursor plus a small slack with nothing at or before it is a gap only if the file's oldest line is after the cursor; when that cannot be told, say "may have been lost"

## Edge Cases
- hoster down for days: podman kept only the last part; the marker says so
- The system clock stepped backwards while hoster was down: never drop lines whose timestamp is earlier than the cursor but that the container wrote after it; prefer a duplicate over a loss and document the choice

## Testing
- Fake-runtime tests: resume with no gap, equal-timestamp lines, rotation gap, exited-while-down, stale cursor cleanup
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§1), ADR-0044 (`k8s-file` size cap), ADR-0034
- `src/orchestrator/reconcile.rs`, `src/runtime/podman.rs`
**Changed while implementing (2026-11-12):** followers no longer pass podman a `since`: every
read starts at the top of the container's log (at most the 10 MiB `k8s-file` cap of ADR-0044)
and skips by position up to and including the cursor's line, then takes everything after it
whatever its stamp. That is what makes the clock-stepped-back edge case lossless (podman's
`since` filters every line by its stamp, so it would drop those lines) and makes gap detection
exact: a log whose first line is already past the cursor was truncated over unread lines
("were lost" marker); a cursor line missing further down gives a "may have been lost" marker.
The cursor stores `nth`, the line's place in the run of consecutive lines with its stamp
(`migrations_logs/0002_log_cursor_nth`, existing cursors default to 1, 7n3's rule). The startup
pass follows every managed container podman lists, running or not, before it resolves anything;
`retire` waits up to 2 s (`LogFollowers::drained`) for a stopped container's follower to finish
before removing it, so an exited leftover's last lines are read. Cursors are deleted through the
recorder (one writer, `LogSink::try_forget_cursor`) when a container is forgotten, and by a sweep
the followers run at startup and hourly (`LogFollowers::sweep_hourly`) rather than in 5d4's
retention pass, since the sweep needs the runtime's container list.
