-- Image digests a stack revision pins its third-party `repo:tag` images to (ADR-0025): resolved
-- once when the revision is applied, so deploys of the revision only ever use stored digests.
-- CI-built images have no row; their digests come from the deploy (webhook, poller, UI).
CREATE TABLE revision_images (
    revision_id INTEGER NOT NULL REFERENCES stack_revisions (id) ON DELETE CASCADE,
    service     TEXT    NOT NULL,
    digest      TEXT    NOT NULL,
    PRIMARY KEY (revision_id, service)
) STRICT;
