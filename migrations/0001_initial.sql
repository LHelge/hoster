-- Initial state store schema (ADR-0022, docs/architecture.md §5).
--
-- Timestamps are milliseconds since the Unix epoch (UTC). Every child table cascades from
-- `stacks`, so removing a stack removes its history, pointers and port reservations with it.

-- One row per stack. `last_generation` is the per-stack generation counter (§5.1): each
-- deployment attempt that reaches Validating takes the next value.
CREATE TABLE stacks (
    name            TEXT    PRIMARY KEY NOT NULL,
    last_generation INTEGER NOT NULL DEFAULT 0 CHECK (last_generation >= 0),
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
) STRICT;

-- Every stack file applied through the UI (ADR-0027), verbatim, with the SHA-256 of the text.
CREATE TABLE stack_revisions (
    id         INTEGER PRIMARY KEY NOT NULL,
    stack      TEXT    NOT NULL REFERENCES stacks (name) ON DELETE CASCADE,
    revision   INTEGER NOT NULL CHECK (revision > 0),
    toml       TEXT    NOT NULL,
    hash       TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (stack, revision)
) STRICT;

-- One deployment attempt (§5.1). `generation` is set once it is assigned in Validating;
-- `committed_at` is set by the commit-point transaction that writes the service pointers, so
-- crash recovery (§5.4) can tell whether the pointers were written.
CREATE TABLE deployments (
    id           INTEGER PRIMARY KEY NOT NULL,
    stack        TEXT    NOT NULL REFERENCES stacks (name) ON DELETE CASCADE,
    revision_id  INTEGER REFERENCES stack_revisions (id) ON DELETE SET NULL,
    generation   INTEGER CHECK (generation > 0),
    state        TEXT    NOT NULL,
    source       TEXT    NOT NULL,
    error        TEXT,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    committed_at INTEGER,
    finished_at  INTEGER
) STRICT;

CREATE INDEX deployments_by_stack ON deployments (stack, id);
CREATE INDEX deployments_unfinished ON deployments (id) WHERE finished_at IS NULL;

-- The target digest set of a deployment: service -> image digest.
CREATE TABLE deployment_targets (
    deployment_id INTEGER NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    service       TEXT    NOT NULL,
    digest        TEXT    NOT NULL,
    PRIMARY KEY (deployment_id, service)
) STRICT;

-- Transition log of every deployment: the crash-recovery record (§5.4) and the UI history.
CREATE TABLE deployment_events (
    id            INTEGER PRIMARY KEY NOT NULL,
    deployment_id INTEGER NOT NULL REFERENCES deployments (id) ON DELETE CASCADE,
    from_state    TEXT,
    to_state      TEXT    NOT NULL,
    message       TEXT,
    at            INTEGER NOT NULL
) STRICT;

CREATE INDEX deployment_events_by_deployment ON deployment_events (deployment_id, id);

-- Per-service current pointers: the durable truth of what is routed (§5.1, §5.4). Written only
-- by the commit-point transaction.
CREATE TABLE services (
    stack         TEXT    NOT NULL REFERENCES stacks (name) ON DELETE CASCADE,
    name          TEXT    NOT NULL,
    generation    INTEGER NOT NULL CHECK (generation > 0),
    digest        TEXT    NOT NULL,
    config_hash   TEXT    NOT NULL,
    deployment_id INTEGER REFERENCES deployments (id) ON DELETE SET NULL,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (stack, name)
) STRICT;

-- The loopback ports of a service's current pointer: one per distinct container port its routes
-- use (stack-format §2.3), none for services without routes. Part of the pointer, so written by
-- the same commit-point transaction.
CREATE TABLE service_ports (
    stack          TEXT    NOT NULL,
    service        TEXT    NOT NULL,
    container_port INTEGER NOT NULL CHECK (container_port BETWEEN 1 AND 65535),
    host_port      INTEGER NOT NULL UNIQUE CHECK (host_port BETWEEN 1 AND 65535),
    PRIMARY KEY (stack, service, container_port),
    FOREIGN KEY (stack, service) REFERENCES services (stack, name) ON DELETE CASCADE
) STRICT;

-- Loopback upstream ports handed out to routed services (ADR-0026).
CREATE TABLE port_allocations (
    port           INTEGER PRIMARY KEY NOT NULL CHECK (port BETWEEN 1 AND 65535),
    stack          TEXT    NOT NULL REFERENCES stacks (name) ON DELETE CASCADE,
    service        TEXT    NOT NULL,
    container_port INTEGER NOT NULL CHECK (container_port BETWEEN 1 AND 65535),
    generation     INTEGER NOT NULL CHECK (generation > 0),
    allocated_at   INTEGER NOT NULL
) STRICT;

-- Small items that need no table of their own (allocator cursor, poller state, ...).
CREATE TABLE kv (
    key        TEXT    PRIMARY KEY NOT NULL,
    value      TEXT    NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;
