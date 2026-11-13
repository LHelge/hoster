---
id: aqy
title: "OTLP: span table and OTLP log record columns in logs.db"
status: done
priority: P2
created: "2026-11-12T19:36:02.163756928Z"
updated: "2026-11-13T17:22:43.169750039Z"
tags:
  - store
  - observability
depends_on:
  - yjp
parent: xv9
attempts: 1
---

## Summary
Storage for what the receiver accepts: a `spans` table in `logs.db` and what `log_records` needs for OTLP log records, with retention and the size cap extended to spans. Drafted on 2026-11-12 from the owner's answers to OQ-42 (logs and traces, spans kept 3 days); checked against ADR-0069 on 2026-11-13.

## Acceptance Criteria
- [ ] A reversible migration of the log store's migrator adds `spans`: trace id, span id, parent span id (nullable), stack, service, name, kind, start (µs since the epoch), duration (µs), status (unset/ok/error) and status message, attributes, resource and events (JSON text), and the time it was received
- [ ] Indexes: `(trace_id)` for loading a trace, `(stack, start)` and `(start)` for the trace list over a range, roots findable cheaply (a partial index on rows without a parent, or a flag)
- [ ] `log_records` takes OTLP records: stream gains `otlp`; OTLP attributes go into fields; trace and span ids (already nullable) are filled; severity text and number map onto the levels as the line parser does
- [ ] Span and record times are the ones the payload gives; the time received is stored beside them (ADR-0069 §6)
- [ ] Retention: spans older than 3 days are deleted by the hourly sweep; the 1 GiB size pass deletes the oldest hour of spans with the other tables
- [ ] Write path through the recorder: `try_span` beside `try_record`, same batching and drop counting
- [ ] Read path: spans by trace id; root spans over a range and scope (the trace list task extends it)
- [ ] The request row gains what the proxy's two spans are derived from (ADR-0069 §9): the server span id, the client span id and the time the upstream request started, in a reversible migration, nullable for rows written before it. `buj` fills them

## Implementation Notes
- Times in µs: spans can be shorter than a millisecond (the mockup shows µs)
- Attributes stored as OTLP gives them, flattened to a JSON object of `key: value`; arrays and nested maps kept as JSON

## Testing
- Migration up/down test; retention and size tests extended with spans; trace lookup
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0069 (§6, §7, §9), ADR-0067 (§2, §4, §7)
- The log store tasks `7ne`, `r8w`, `5d4`