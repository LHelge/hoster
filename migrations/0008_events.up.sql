-- The event log (ADR-0055): one row per deploy outcome, container restart or health change,
-- certificate outcome and operator or system action, for the dashboard's Recent events feed and
-- the Events page. `stack` has no foreign key, so a stack's history outlives its removal.
-- `kind` and `severity` are the store's `EventKind` and `Severity` as text; `subject` is the
-- service, host or deployment the event links to; `meta` is the short mono line (sha, digest,
-- exit code, version). Rows older than 30 days are deleted daily. Timestamps are milliseconds
-- since the Unix epoch (UTC), as in 0001.
CREATE TABLE events (
    id       INTEGER PRIMARY KEY NOT NULL,
    at       INTEGER NOT NULL,
    kind     TEXT    NOT NULL,
    severity TEXT    NOT NULL,
    stack    TEXT,
    subject  TEXT,
    message  TEXT    NOT NULL,
    meta     TEXT
) STRICT;

CREATE INDEX events_by_time ON events (at);
CREATE INDEX events_by_stack ON events (stack, at);
