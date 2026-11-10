-- Email notifications (ADR-0058 §5, §6, §8, §9): the run-time settings row and the notifier's
-- state. The provider's API key is not here; it is a sealed secret (ADR-0029).

-- The notification settings, one typed row edited in Settings → Notifications. No row means
-- notifications are not configured. `provider` is the `notify::ProviderKind` name (`resend`);
-- the addresses are `notify::MailAddress` text. A row may be saved disabled before it is
-- complete, but never enabled without both addresses.
CREATE TABLE notification_settings (
    id             INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    provider       TEXT    NOT NULL,
    from_address   TEXT,
    recipient      TEXT,
    enabled        INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    also_successes INTEGER NOT NULL CHECK (also_successes IN (0, 1)),
    updated_at     INTEGER NOT NULL,
    CHECK (enabled = 0 OR (from_address IS NOT NULL AND recipient IS NOT NULL))
) STRICT;

-- The notifier's single row: the id of the last event it has handled (NULL until it is first
-- placed at the newest event; switching notifications on moves it to the newest event), and the
-- last delivery: when, the provider's id for a sent mail, or the error text of a failed one.
CREATE TABLE notifier_state (
    id             INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    cursor         INTEGER,
    delivered_at   INTEGER,
    delivery_id    TEXT,
    delivery_error TEXT,
    CHECK (delivered_at IS NOT NULL OR (delivery_id IS NULL AND delivery_error IS NULL)),
    CHECK (delivery_id IS NULL OR delivery_error IS NULL)
) STRICT;

-- The open problems the notifier has mailed, one per (kind, stack, subject), with the event and
-- time of the last mail about it (for the 24-hour reminder). A recovery deletes the row. An
-- event without a stack or subject is keyed with '' in its place, so the key stays unique.
CREATE TABLE notified_problems (
    kind        TEXT    NOT NULL,
    stack       TEXT    NOT NULL,
    subject     TEXT    NOT NULL,
    event_id    INTEGER NOT NULL,
    notified_at INTEGER NOT NULL,
    PRIMARY KEY (kind, stack, subject)
) STRICT;
