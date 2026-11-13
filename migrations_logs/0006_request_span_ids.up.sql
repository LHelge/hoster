-- What the proxy's two spans of a request are built from when a trace is read (ADR-0069 §9):
-- the server span's id and the client (upstream) span's id, 16 hex digits each, and when the
-- upstream request started, in microseconds since the epoch like span times. The trace id is
-- the request id. NULL for rows written before these columns existed, and for an upstream
-- request that never started.
ALTER TABLE requests ADD COLUMN server_span_id TEXT;
ALTER TABLE requests ADD COLUMN client_span_id TEXT;
ALTER TABLE requests ADD COLUMN upstream_start INTEGER;
