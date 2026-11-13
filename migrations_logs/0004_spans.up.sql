-- Spans received over OTLP (ADR-0069 §6, §7). One row per span a container exported; the
-- proxy's own two spans per request are not stored here but derived from its request row
-- (ADR-0069 §9). Stack and service are the ones the exporter's token names, never what the
-- payload claims. Kept 3 days, and within the size cap with the other tables.
--
-- `trace_id` is 32 hex digits and `span_id` and `parent_span_id` 16, as W3C trace context
-- writes them; `parent_span_id` is NULL for a span without a parent. `start` (as the payload
-- gives it) and `duration` are microseconds, since spans can be shorter than a millisecond;
-- `received` is when hoster accepted the span, in milliseconds like every other time in this
-- file. `kind` is `unspecified`, `internal`, `server`, `client`, `producer` or `consumer`;
-- `status` is `unset`, `ok` or `error`. `attributes` and `resource` are JSON objects and
-- `events` a JSON array, as text; NULL when the payload has none.
CREATE TABLE spans (
    id             INTEGER PRIMARY KEY,
    trace_id       TEXT    NOT NULL,
    span_id        TEXT    NOT NULL,
    parent_span_id TEXT,
    stack          TEXT    NOT NULL,
    service        TEXT    NOT NULL,
    name           TEXT    NOT NULL,
    kind           TEXT    NOT NULL,
    start          INTEGER NOT NULL,
    duration       INTEGER NOT NULL,
    status         TEXT    NOT NULL,
    status_message TEXT,
    attributes     TEXT,
    resource       TEXT,
    events         TEXT,
    received       INTEGER NOT NULL
) STRICT;

-- One trace's spans.
CREATE INDEX spans_by_trace ON spans (trace_id);
-- A stack's spans over a range, and every stack's (the trace list).
CREATE INDEX spans_by_stack ON spans (stack, start);
CREATE INDEX spans_by_start ON spans (start);
-- The roots, the same two ways: partial, so only spans without a parent are in them.
CREATE INDEX spans_roots_by_stack ON spans (stack, start) WHERE parent_span_id IS NULL;
CREATE INDEX spans_roots_by_start ON spans (start) WHERE parent_span_id IS NULL;
