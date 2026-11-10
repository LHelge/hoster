---
id: q55
title: "Store: notification settings, delivery state and the delivery-failed event"
status: done
priority: P3
created: "2026-11-09T23:22:59.495412641Z"
updated: "2026-11-10T00:29:51.983134240Z"
tags:
  - store
  - notify
depends_on:
  - rja
  - mxq
parent: c5s
attempts: 1
---

## Summary
Persist what notifications need, as decided in ADR-0058: the settings row, the notifier's cursor and de-duplication state, the last delivery result, and a new event kind for a failed delivery.

## Acceptance Criteria
- [ ] A reversible migration (`sqlx migrate add -r`) adds the `notification_settings` single-row table and the notifier state tables; the `.down.sql` restores the previous schema exactly (the store tests enforce it).
- [ ] Settings: provider (typed enum stored as text), from address, recipient, enabled, "also successes". One row at most; absent means "not configured". From and recipient are typed (a validated mail address newtype with `FromStr`/`Display`), not bare strings.
- [ ] Notifier state: the event cursor, the per-(kind, stack, subject) record of what was last notified and when, and the last delivery (time, result, error text).
- [ ] `EventKind` gains `NotificationFailed` (severity `warning`), with its severity, label and filter entry wherever `EventKind::ALL` is used (Events page filters, `event_row.html`).
- [ ] Store methods to read and write the settings, advance the cursor together with the de-duplication record in one transaction, and record a delivery result.
- [ ] The API key is not in these tables; it lives in the secret store (ADR-0029).

## Implementation Notes
- The instance-name task (`mxq`) adds the first single-row settings table; use the same shape.
- Follow `src/store/events.rs` and `src/store/webhook.rs` for module shape, and `src/store/types.rs` for typed columns.
- Down-migrating (ADR-0052 rollback) drops the settings. That is acceptable, as for the event log, but say so in the migration's comment.

## Testing
- Store tests for round-tripping the settings, the cursor transaction and the up/down migration.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the tests pass.

## References
- ADR-0058, ADR-0029, ADR-0052, ADR-0055
- `src/store/`, `migrations/`