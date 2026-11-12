-- Reverts 0003_requests_by_request_id: a lookup by request id scans the table again.
DROP INDEX requests_by_request_id;
