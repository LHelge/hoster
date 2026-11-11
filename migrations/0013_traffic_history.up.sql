-- Traffic history (ADR-0062): per-route request counts in 1-minute buckets kept 24 hours and
-- 1-hour buckets kept 60 days, both of one shape. `start` is the bucket start in milliseconds
-- since the Unix epoch (UTC), aligned to a minute or an hour. The key columns are NOT NULL
-- with '' for none, so an upsert on the primary key matches: hoster's own routes have
-- `stack = ''` and the reserved purpose as `service`, and unrouted requests are the series with
-- every key column ''. `stack` has no foreign key, so a removed stack's history stays until
-- retention removes it. One count per status class (`none` is no response sent), the requests
-- that failed because of their upstream, and `latency`, the versioned latency histogram blob.
-- Only buckets with requests have rows.
CREATE TABLE traffic_minutes (
    start           INTEGER NOT NULL,
    host            TEXT    NOT NULL,
    prefix          TEXT    NOT NULL,
    stack           TEXT    NOT NULL,
    service         TEXT    NOT NULL,
    none            INTEGER NOT NULL,
    s1xx            INTEGER NOT NULL,
    s2xx            INTEGER NOT NULL,
    s3xx            INTEGER NOT NULL,
    s4xx            INTEGER NOT NULL,
    s5xx            INTEGER NOT NULL,
    upstream_errors INTEGER NOT NULL,
    latency         BLOB    NOT NULL,
    PRIMARY KEY (start, host, prefix, stack, service)
) STRICT;

CREATE INDEX traffic_minutes_by_stack ON traffic_minutes (stack, start);

CREATE TABLE traffic_hours (
    start           INTEGER NOT NULL,
    host            TEXT    NOT NULL,
    prefix          TEXT    NOT NULL,
    stack           TEXT    NOT NULL,
    service         TEXT    NOT NULL,
    none            INTEGER NOT NULL,
    s1xx            INTEGER NOT NULL,
    s2xx            INTEGER NOT NULL,
    s3xx            INTEGER NOT NULL,
    s4xx            INTEGER NOT NULL,
    s5xx            INTEGER NOT NULL,
    upstream_errors INTEGER NOT NULL,
    latency         BLOB    NOT NULL,
    PRIMARY KEY (start, host, prefix, stack, service)
) STRICT;

CREATE INDEX traffic_hours_by_stack ON traffic_hours (stack, start);
