-- Stack removal keeps the deployment history (ADR-0030): a removed stack's row stays, marked
-- with `removed_at`, so its revisions, deployments and transition logs survive. Its service
-- pointers and port reservations are deleted on removal. Applying the stack again clears the
-- mark; generations and revision numbers carry on where they stopped, so container names never
-- repeat.
ALTER TABLE stacks ADD COLUMN removed_at INTEGER;
