---
id: aqy
title: "OTLP: span table and OTLP log record columns in logs.db"
status: open
priority: P2
created: "2026-11-12T19:36:02.163756928Z"
updated: "2026-11-12T19:36:02.163756928Z"
tags:
  - store
  - observability
depends_on:
  - yjp
parent: xv9
---

## Summary
Storage for what the receiver accepts: a `spans` table in `logs.db` and what `log_records` needs for OTLP log records, with retention and the size cap extended to spans. Drafted on 2026-11-12 from the owner's answers to OQ-42 (logs and traces, spans kept 3 days); check it against the ADR from `yjp` and amend it in the same commit if they differ.

## Acceptance Criteria
- [ ] A reversible migration of the log store's migrator adds `spans`: trace id, span id, parent span id (nullable), stack, service, name, kind, start (µs since the epoch), duration (µs), status (unset/ok/error) and status message, attributes, resource and events (JSON text), and the time it was received
- [ ] Indexes: `(trace_id)` for loading a trace, `(stack, start)` and `(start)` for the trace list over a range, roots findable cheaply (a partial index on rows without a parent, or a flag)
- [ ] `log_records` takes OTLP records: stream gains `otlp`; OTLP attributes go into fields; trace and span ids (already nullable) are filled; severity text and number map onto the levels as the line parser does
- [ ] Retention: spans older than 3 days are deleted by the hourly sweep; the 1 GiB size pass deletes the oldest hour of spans with the other tables
- [ ] Write path through the recorder: `try_span` beside `try_record`, same batching and drop counting
- [ ] Read path: spans by trace id; root spans over a range and scope (the trace list task extends it)
- [ ] If the ADR picks option A of question 8 (proxy spans derived from request rows), the request row gains the columns it needs here unless `buj` already added them; option B adds nothing to `requests`

## Implementation Notes
- Times in µs: spans can be shorter than a millisecond (the mockup shows µs)
- Attributes stored as OTLP gives them, flattened to a JSON object of `key: value`; arrays and nested maps kept as JSON

## Testing
- Migration up/down test; retention and size tests extended with spans; trace lookup
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- OQ-42 (questions 5, 7, 8), the ADR from `yjp`, ADR-0067 (§2, §4, §7)
- The log store tasks `7ne`, `r8w`, `5d4`