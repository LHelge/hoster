-- A request row by its request id (ADR-0067 §5): a container record that logged the id the
-- proxy gave its request links to the row, whenever the request arrived.
CREATE INDEX requests_by_request_id ON requests (request_id);
