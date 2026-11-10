-- Reverts 0010_notifications. The notification settings and the notifier's state go with the
-- tables (ADR-0058: acceptable; the operator configures notifications again after a rollback
-- past this release). The API key stays in `secrets`, which predates this migration.
-- The older release's `EventKind` has no `notification_failed`, and a row it cannot decode
-- would break its Events page and dashboard feed, so those events go too.
DELETE FROM events WHERE kind = 'notification_failed';
DROP TABLE notified_problems;
DROP TABLE notifier_state;
DROP TABLE notification_settings;
