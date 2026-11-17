---
id: "3ku"
title: "Log follower: read trace and span ids from JSON stdout lines (ADR-0072)"
status: open
priority: P2
created: "2026-11-17T20:53:27.227572608Z"
updated: "2026-11-17T20:53:27.227572608Z"
tags:
  - logs
  - observability
---

## Summary
Implement ADR-0072: the container log follower fills a record's `trace_id` and `span_id` columns from fixed keys of a JSON line, so stdout records link to their trace like OTLP records (the links, the `?trace=` filter and the span details of epic `qnh` already work for any record that has ids).

## Acceptance Criteria
- [ ] Trace id from `trace_id` or `traceId`, span id from `span_id` or `spanId`, first match wins; looked for at the top level and then inside a top-level `fields` object
- [ ] Only well-formed W3C ids are taken (32 / 16 hex digits, not all zero; upper case accepted, stored lower case); anything else stays a field
- [ ] A span id is stored only beside a valid trace id from the same line; a trace id alone is stored
- [ ] A mapped key is removed from the record's fields; a `fields` object left empty by that is handled as an empty one is today
- [ ] Text lines, OTLP records and hoster's own records are unchanged; no migration
- [ ] `site/content/wiki/tracing.md` ("pick one: stdout or OTLP for logs", "Traces and log records") and `logging.md` say which keys are read; `docs/architecture.md` where it describes the parser

## Testing
- Parser tests: each key and position, both spellings, mixed validity, span without trace, upper case, all-zero ids, non-string values, keys left in fields when not mapped
- A handler or store test: a stdout record stored through the follower's mapping is found by the `?trace=` filter and links to its trace
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass; `aphid build`

## References
- ADR-0072, ADR-0067 (§3, §4), ADR-0069 (§6)
- `src/logs/follower.rs`, `src/web/logs.rs`