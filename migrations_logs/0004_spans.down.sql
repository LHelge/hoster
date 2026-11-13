-- Reverts 0004_spans: the spans go with the table, as logs are not kept across a rollback.
DROP INDEX spans_roots_by_start;
DROP INDEX spans_roots_by_stack;
DROP INDEX spans_by_start;
DROP INDEX spans_by_stack;
DROP INDEX spans_by_trace;
DROP TABLE spans;
