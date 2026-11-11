-- Third-party image updates (ADR-0061 §6–§8): the update checker's settings and what it found.
-- The time of its last pass is the `updates.last_pass` key-value item.

-- The update checker's settings, one typed row edited in Settings → Updates (ADR-0058 §1). No
-- row means the defaults: a check every 6 hours.
CREATE TABLE update_settings (
    id            INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    interval_secs INTEGER NOT NULL CHECK (interval_secs BETWEEN 3600 AND 604800),
    updated_at    INTEGER NOT NULL
) STRICT;

-- The last check of each checkable service: the image (`registry/repository`), tag and digest
-- of the active revision it was checked for, when, and why it failed if it did. A row whose
-- image, tag or digest no longer match the active revision is stale; the next check replaces it.
CREATE TABLE image_checks (
    stack      TEXT    NOT NULL,
    service    TEXT    NOT NULL,
    image      TEXT    NOT NULL,
    tag        TEXT    NOT NULL,
    digest     TEXT    NOT NULL,
    checked_at INTEGER NOT NULL,
    error      TEXT,
    PRIMARY KEY (stack, service)
) STRICT;

-- The updates found for a checked service, the highest of each kind: `digest` (the tag moved;
-- `digest` is the new one), or `patch`, `minor` and `major` (a newer version tag in the same
-- line). `first_seen_at` is when this candidate was first found.
CREATE TABLE image_updates (
    stack         TEXT    NOT NULL,
    service       TEXT    NOT NULL,
    kind          TEXT    NOT NULL CHECK (kind IN ('digest', 'patch', 'minor', 'major')),
    tag           TEXT    NOT NULL,
    digest        TEXT,
    first_seen_at INTEGER NOT NULL,
    PRIMARY KEY (stack, service, kind),
    FOREIGN KEY (stack, service) REFERENCES image_checks (stack, service) ON DELETE CASCADE,
    CHECK ((kind = 'digest') = (digest IS NOT NULL))
) STRICT;
