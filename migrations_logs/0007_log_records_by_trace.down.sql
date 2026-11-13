-- Reverts 0007_log_records_by_trace: a trace's records are found by scanning the table again.
DROP INDEX log_records_by_trace;
