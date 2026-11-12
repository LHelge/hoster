-- The log store (ADR-0067, amended by ADR-0068): `logs.db`, a second SQLite file beside
-- `hoster.db` with its own migrations. Nothing here refers to `hoster.db` and there are no
-- foreign keys, so a removed stack's records and requests stay until retention removes them.
-- Times are milliseconds since the Unix epoch (UTC). The logs UI pages and follows by id, so
-- ids must never be reused: retention always keeps a table's newest row, as the events table's
-- pruning does (AUTOINCREMENT would leave `sqlite_sequence` behind, which a down script cannot
-- drop).

-- One record per line a managed container printed (`stdout`, `stderr`) or per event hoster
-- logged itself at info and above (`hoster`, ADR-0068). `stack` is NULL only for hoster's own
-- records that name no stack; `generation` is NULL for those too. `level` is NULL for text
-- lines (ADR-0067 §3: no guessing). `fields` is the JSON object of a JSON line's other keys.
-- `cut` marks a line cut at 16 KiB; `marker` marks a record hoster wrote about lines it lost
-- (rate cap, a gap while it was down), whose message says what. `trace_id` (32 hex digits)
-- and `span_id` (16) stay NULL until hoster has trace context (OQ-42).
CREATE TABLE log_records (
    id         INTEGER PRIMARY KEY,
    time       INTEGER NOT NULL,
    stack      TEXT,
    service    TEXT    NOT NULL,
    generation INTEGER,
    stream     TEXT    NOT NULL,
    level      TEXT,
    message    TEXT    NOT NULL,
    fields     TEXT,
    cut        INTEGER NOT NULL DEFAULT 0,
    marker     INTEGER NOT NULL DEFAULT 0,
    trace_id   TEXT,
    span_id    TEXT
) STRICT;

-- A stack over a span (the stack logs tab), and every stack over a span (the Telemetry page).
CREATE INDEX log_records_by_stack ON log_records (stack, time);
CREATE INDEX log_records_by_time ON log_records (time);

-- One row per request the proxy routed to a stack (ADR-0067 §5). `time` is when the request
-- arrived; `header_ms` (until the upstream's response header) is NULL when none was received;
-- `total_ms` runs until the last byte was sent or the request ended. `client` is the address
-- the proxy saw. `request_id` is 32 hex digits, the shape of a W3C trace id (OQ-42).
CREATE TABLE requests (
    id              INTEGER PRIMARY KEY,
    time            INTEGER NOT NULL,
    host            TEXT    NOT NULL,
    prefix          TEXT    NOT NULL,
    stack           TEXT    NOT NULL,
    service         TEXT    NOT NULL,
    method          TEXT    NOT NULL,
    path            TEXT    NOT NULL,
    status          INTEGER NOT NULL,
    header_ms       INTEGER,
    total_ms        INTEGER NOT NULL,
    bytes           INTEGER NOT NULL,
    client          TEXT    NOT NULL,
    upstream_failed INTEGER NOT NULL,
    request_id      TEXT    NOT NULL
) STRICT;

CREATE INDEX requests_by_stack ON requests (stack, time);
CREATE INDEX requests_by_time ON requests (time);

-- Per followed container, podman's timestamp (nanoseconds since the epoch) of the last line
-- handed to the store, so a follower resumes there after hoster restarts (ADR-0067 §1).
CREATE TABLE log_cursors (
    container TEXT    PRIMARY KEY,
    time_ns   INTEGER NOT NULL
) STRICT;
