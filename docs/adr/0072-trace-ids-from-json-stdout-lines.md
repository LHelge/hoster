# ADR-0072: The log follower reads trace and span ids from fixed keys of a JSON line

- Status: Accepted (resolves OQ-47; amends ADR-0067)
- Date: 2026-11-17

## Context

A log record has nullable trace and span id columns (ADR-0067 §4). Since epic `qnh`, a record
that has them links to its trace from the logs views, is found by the Logs tab's `?trace=`
filter and by the waterfall's "logs in trace", and is listed under its span in the span
details. Only the OTLP receiver fills the columns (ADR-0069 §6). ADR-0067 §3 maps a JSON
line's level and message and keeps every other key as a field, and its §4 says the ids "stay
empty until OQ-42"; ADR-0069 then decided the OTLP side only. So an app that sends spans over
OTLP and logs JSON to stdout with `trace_id` and `span_id` in each line, which is the usual
setup with `tracing` and a JSON formatter and one of the two the tracing guide offers, gets
no link between its lines and its traces.

The owner chose option B of OQ-47 on 2026-11-17: read fixed keys. The details below are the
recommendation that came with it.

## Decision

1. **Keys.** For a line that parses as a JSON object, the follower reads the trace id from
   `trace_id` or `traceId` and the span id from `span_id` or `spanId`, first match wins. Each
   key is looked for at the top level of the object and then inside a top-level `fields`
   object, where `tracing`'s JSON formatter puts an event's own fields (as ADR-0067 §3 already
   reads `fields.message`).
2. **Well-formed ids only.** A trace id is 32 hex digits and a span id 16, as W3C Trace
   Context writes them, not all zero. Upper-case digits are accepted and stored in lower case.
   Any other value (a number, a shorter id, a vendor's decimal id) is not an id: it is left
   where it was, as a field.
3. **A span id needs a trace id.** A valid trace id is stored on its own. A span id is stored
   only beside a valid trace id from the same line; alone it stays a field, since nothing can
   look a span up without its trace.
4. **Mapped keys leave the fields.** A key whose value was stored in a column is removed from
   the record's fields, as the level and message keys are, so the id is shown once. The
   opened record lists `trace_id` and `span_id` from the columns.
5. **No setting.** The keys are fixed. There is no stack-file field for them.
6. **Scope.** This is the mapping of ADR-0067 §3 for container lines. Text lines are not
   searched for ids. Records that arrive over OTLP and hoster's own records (ADR-0068,
   ADR-0069) are unchanged. It applies to lines stored from the release that has it; stored
   records are not rewritten.

This amends ADR-0067 §3 (two more mapped record fields) and the sentence of its §4 that leaves
the ids empty. There is no schema change: the columns and the `log_records_by_trace` index
exist.

## Alternatives considered

- **Leave it** (OQ-47 option A): linked logs would need OTLP logs. That makes the stdout half
  of the tracing guide a dead end for anyone who also sends spans.
- **Configurable keys per service** (option C): covers loggers with other key names, at the
  cost of a stack-format field to parse, document and keep. It can be added later without
  changing this mapping.
- **More key names now** (`otelTraceID`, `dd.trace_id`, `logging.googleapis.com/trace`): each
  has its own id format or prefix. None is in use on the owner's hosts.
- **Keeping the keys in the fields as well**: the id would show twice in an opened record, and
  a field filter and the trace filter would be two ways to the same records.
- **Parsing a `traceparent` field**: no common logger writes one into a log line.

## Consequences

- An app's stdout JSON lines that carry W3C ids link to their trace like OTLP records do,
  without the app sending logs over OTLP.
- A line can claim any trace id, as an OTLP record can. The ids are links, not access
  control: a record is shown under its own stack and service whatever trace it names.
- A field filter on `trace_id` (a clicked field chip in an older URL) no longer matches new
  records, whose id has moved to the column; the `?trace=` filter is the way to them.
- The tracing guide's "pick one: stdout or OTLP for logs" no longer costs the links.
