-- Images hoster pulled (ADR-0041, correction of 2026-11-07). A pull adds no label to an image, so
-- this table is how image garbage collection tells hoster's images from everything else in the
-- podman store: only rows here are ever pruned, and only when no service's retention set
-- (current digest plus the last successful ones) names the digest. `repository` is
-- `registry/repository`, so `repository@digest` is the reference hoster pulled. Timestamps are
-- milliseconds since the Unix epoch (UTC), as in 0001.
CREATE TABLE images (
    repository TEXT    NOT NULL,
    digest     TEXT    NOT NULL,
    pulled_at  INTEGER NOT NULL,
    PRIMARY KEY (repository, digest)
) STRICT;
