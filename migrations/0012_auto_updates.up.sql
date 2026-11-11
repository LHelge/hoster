-- Opt-in auto-update (ADR-0061 §10): the target last tried for each service, so a target is
-- tried once. A failed deployment, a rollback or a manual edit back to the old tag leaves the
-- service alone until the checker finds a newer target. `digest` is set for a moved tag and
-- NULL for a version, which is resolved when it is applied.
CREATE TABLE auto_update_attempts (
    stack        TEXT    NOT NULL,
    service      TEXT    NOT NULL,
    tag          TEXT    NOT NULL,
    digest       TEXT,
    attempted_at INTEGER NOT NULL,
    PRIMARY KEY (stack, service)
) STRICT;
