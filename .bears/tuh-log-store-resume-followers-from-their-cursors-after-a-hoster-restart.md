---
id: tuh
title: "Log store: resume followers from their cursors after a hoster restart"
status: open
priority: P2
created: "2026-11-12T19:34:02.707030369Z"
updated: "2026-11-12T19:34:02.707030369Z"
tags:
  - observability
  - runtime
depends_on:
  - "7n3"
parent: f8z
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