---
id: "9x4"
title: Traces and logs link both ways
status: open
priority: P2
created: "2026-11-12T19:37:50.238037958Z"
updated: "2026-11-12T19:37:56.284179809Z"
tags:
  - web
  - observability
depends_on:
  - jpq
  - ckc
parent: qnh
---

## Summary
Connect the two Telemetry tabs and the stack logs tab through trace and span ids, as the mockup does: a record with a trace id links to its trace and span; a trace links to its records; an access-log row links to its trace.

## Acceptance Criteria
- [ ] The shared log view (epic `psq`) gains a TRACE column in both scopes: the first 8 hex digits of the trace id with "→", linking to `/telemetry/traces?trace=<id>&span=<span id>`; "—" for records without one
- [ ] A `?trace=<id>` filter in the shared log view, shown as a removable chip "trace_id = 4bf92f35…"; it uses the store's trace-id lookup, not a scan
- [ ] The waterfall's "logs in trace (N) →" links to the Logs tab with that filter and all levels
- [ ] An expanded record shows `trace_id`, `span_id` and `service.name` with its fields, as the mockup does
- [ ] The access-log source (`bq6`) links each row to its trace once the proxy sends trace context (`buj`); before that the link is absent
- [ ] Records from OTLP and stdout records that carry ids both link; the stack logs tab gets the column too

## Implementation Notes
- The store already has trace and span ids on records (ADR-0067 §4); add an index on `log_records(trace_id)` in the log store's migrations here if `aqy` did not

## Testing
- Handler tests: column and links, the trace filter chip, "logs in trace" count, access-log link present only with trace context
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- `docs/design/Telemetry.dc.html` (logs ↔ traces), ADR-0067 (§4)
- Tasks `yn7`, `ckc`, `bq6`, `buj`, `jpq`