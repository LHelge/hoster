-- Reverts 0006_request_span_ids: request rows lose the proxy's span ids, and their traces
-- the proxy's two spans.
ALTER TABLE requests DROP COLUMN upstream_start;
ALTER TABLE requests DROP COLUMN client_span_id;
ALTER TABLE requests DROP COLUMN server_span_id;
