-- A trace's log records, and a span's among them (ADR-0069 §6, §8): the waterfall counts the
-- shown trace's records and lists the selected span's, and the Logs tab's `?trace=` filter
-- reads one trace's records. Partial, so the many records outside any trace (text lines, most
-- JSON lines) are not in it.
CREATE INDEX log_records_by_trace ON log_records (trace_id, span_id) WHERE trace_id IS NOT NULL;
