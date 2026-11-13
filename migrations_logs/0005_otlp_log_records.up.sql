-- Log records received over OTLP (ADR-0069 §6) go into `log_records` with stream `otlp`, their
-- attributes as `fields` and their `trace_id` and `span_id` filled. Their `time` is the one the
-- payload gives; `received` is when hoster accepted them, in milliseconds. NULL for every
-- other record, whose time is already hoster's (podman's stamp, or when hoster logged it).
ALTER TABLE log_records ADD COLUMN received INTEGER;
