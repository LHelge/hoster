---
id: jpq
title: "Trace waterfall: inline span details"
status: open
priority: P2
created: "2026-11-12T19:37:41.668951477Z"
updated: "2026-11-12T19:37:41.668951477Z"
tags:
  - web
  - observability
depends_on:
  - aum
parent: qnh
---

## Summary
Selecting a span (`?span=<id>`) opens its details inline below its row, as in the mockup: timing, ids, status, attributes, resource, events and the log records emitted inside it.

## Acceptance Criteria
- [ ] Title line: service colour, span name, service, kind, status pill; a close link that drops `span` from the URL
- [ ] START (offset from the trace start), DURATION with its share of the trace, SELF TIME (duration minus the union of child intervals), SPAN ID, PARENT ("— (root)" for a root)
- [ ] For a span in error, the status message on its own line
- [ ] ATTRIBUTES as a key/value list; `error.type` and a 5xx `http.response.status_code` in red. RESOURCE as `key=value` chips (`service.name`, `service.version`, `telemetry.sdk.language`, and hoster's stack and generation)
- [ ] EVENTS with name, time offset ("@ 1.85 s (+1.80 s in span)"), their attributes, and `exception.stacktrace` in a `<pre>`; "no events on this span" otherwise
- [ ] LOGS FOR THIS SPAN: records with this span id (time, level, message), each linking to the record in the Logs tab; "none emitted" otherwise
- [ ] When the trace first opens with no `span`, the deepest span in error is selected if any, the root otherwise (the mockup's rule)
- [ ] Every value escaped; long values wrap

## Testing
- Handler tests on the mockup's `t1`/`s9`: every section, self time, default selection, escaping
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- `docs/design/Telemetry.dc.html` (span details), ADR-0054
- The trace query task `txt`, the waterfall task `aum`