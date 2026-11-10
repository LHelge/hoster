---
id: pym
title: "Notifier task: select events, de-duplicate, rate-limit, deliver with retry"
status: done
priority: P3
created: "2026-11-09T23:23:24.824853270Z"
updated: "2026-11-10T00:50:00.090887606Z"
tags:
  - notify
  - orchestrator
depends_on:
  - q55
  - pzp
  - "6qz"
  - mxq
parent: c5s
attempts: 1
---

## Summary
The background task that turns the event log into mail, per ADR-0058: it reads events past its cursor, keeps the ones worth sending, suppresses repeats, enforces the hourly cap, delivers through the configured provider with a short retry, and records failures.

## Acceptance Criteria
- [ ] A notifier task started by the supervisor and stopped on shutdown like the other background tasks. It does nothing while notifications are not configured or disabled, and picks up a settings change without a restart.
- [ ] It reads the `events` table past the persisted cursor, woken by the `UiEvent` broadcast and a slow timer. A lagging broadcast receiver loses nothing, because the table is the source.
- [ ] The cursor starts at the newest event when notifications are first enabled. Events older than the ADR's cut-off are skipped, so a restart after a long outage sends no stale mail.
- [ ] Subjects use the current instance name (task `mxq`).
- [ ] Selection follows the ADR's sets: problems, recoveries, the always-sent informational kinds, and the success kinds only with "also successes". The delivery-failed kind never notifies.
- [ ] De-duplication by (kind, stack, subject): a repeated failure is silent until it recovers or the reminder interval passes. A certificate recovery is sent only for a host whose failure was notified.
- [ ] The global hourly cap; beyond it, one summary mail with the number of suppressed events.
- [ ] Delivery: retry retryable errors with back-off for the ADR's window, then drop. A permanent error drops at once. Either way a delivery-failed event is recorded and the last-delivery state is updated; a success updates it too.
- [ ] The cursor and the de-duplication record advance in one transaction, so a crash mid-batch neither loses nor repeats a mail beyond the one in flight.
- [ ] The selection and de-duplication rules live on a type with no I/O, so they are tested without a store or a provider.

## Edge Cases
- A burst (a stack with many crash-looping services) hits the cap and produces one summary, not silence.
- Settings disabled and re-enabled: the cursor moves to the newest event; nothing from the disabled period is sent.
- The provider is failing permanently (revoked key): one delivery-failed event per reminder interval, not one per event.
- Shutdown during a retry wait ends promptly.

## Testing
- Unit tests for selection, de-duplication, the reminder interval and the cap with a fake clock.
- A task-level test with a fake provider and a real store: problem then recovery, retry then success, retry then drop with the failure event, restart between events.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the tests pass.

## References
- ADR-0058 §6–§9, ADR-0028 (crash-loop back-off), ADR-0055
- `docs/architecture.md` §3 (threads and tasks)
- `src/supervisor.rs`, `src/store/events.rs`, `src/web/events/live.rs` (the `UiEvent` stream), `src/registry/poller.rs` (a background task to model on)