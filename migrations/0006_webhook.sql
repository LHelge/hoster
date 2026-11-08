-- The OIDC-authenticated deploy webhook (ADR-0047).

-- Where a webhook deployment came from, taken from the verified token's claims (`tag` from the
-- request body, informational). NULL for every other source.
ALTER TABLE deployments ADD COLUMN source_repository TEXT;
ALTER TABLE deployments ADD COLUMN source_sha TEXT;
ALTER TABLE deployments ADD COLUMN source_ref TEXT;
ALTER TABLE deployments ADD COLUMN source_run_id TEXT;
ALTER TABLE deployments ADD COLUMN source_actor TEXT;
ALTER TABLE deployments ADD COLUMN source_tag TEXT;

-- Token ids (`jti`) already used, each kept until its token can no longer be accepted.
CREATE TABLE webhook_tokens (
    jti        TEXT    PRIMARY KEY NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT;

CREATE INDEX webhook_tokens_by_expiry ON webhook_tokens (expires_at);

-- Webhook attempts with a valid token that were refused, with the token's claims (never the
-- token), so the admin can see which repository, ref and workflow tried.
CREATE TABLE webhook_rejections (
    id               INTEGER PRIMARY KEY NOT NULL,
    stack            TEXT    NOT NULL REFERENCES stacks (name) ON DELETE CASCADE,
    reason           TEXT    NOT NULL,
    repository       TEXT    NOT NULL,
    repository_id    TEXT    NOT NULL,
    git_ref          TEXT    NOT NULL,
    environment      TEXT,
    workflow_ref     TEXT,
    job_workflow_ref TEXT,
    actor            TEXT    NOT NULL,
    run_id           TEXT    NOT NULL,
    at               INTEGER NOT NULL
) STRICT;

CREATE INDEX webhook_rejections_by_stack ON webhook_rejections (stack, id);
