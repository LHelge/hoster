-- A trace's log records (ADR-0069 §6, §8): how many each of its spans has, and one span's
-- records, for the trace waterfall and its span details. Partial, so only records made in a
-- trace (OTLP records, hoster's own records inside a request) are in it.
CREATE INDEX log_records_by_trace ON log_records (trace_id, span_id, time) WHERE trace_id IS NOT NULL;
