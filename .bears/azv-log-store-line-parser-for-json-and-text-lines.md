---
id: azv
title: "Log store: line parser for JSON and text lines"
status: open
priority: P2
created: "2026-11-12T19:32:58.246131282Z"
updated: "2026-11-12T19:32:58.246131282Z"
tags:
  - observability
  - runtime
parent: f8z
---

## Summary
Turn one line a container printed into the parts of a log record, as ADR-0067 §3 specifies: a JSON object is mapped to level, message and fields by the common key spellings, any other line is kept whole with no level. Pure code with no I/O, so it can be built and tested before the follower exists.

## Acceptance Criteria
- [ ] A parsed-line type built with `From<&str>` (or `FromStr`, infallible) yields level (optional), message, fields (optional JSON) and whether the line was cut
- [ ] Level keys `level`, `severity`, `lvl`, first match wins; names in any case (`WARN`, `Warning`, `err`, `fatal`, `critical`, `trace`) and the usual numeric scales (bunyan/pino 10–60, syslog 0–7, OTel severity numbers 1–24) map to `trace`, `debug`, `info`, `warn`, `error`; an unknown value leaves the level empty and keeps the key in fields
- [ ] Message keys `message`, `msg`, then `fields.message` as `tracing-subscriber`'s JSON formatter writes it; the key used is removed from fields, the rest of `fields` is kept
- [ ] Every other key is kept in fields as JSON, `target`, `span` and `spans` included
- [ ] A JSON value that is not an object (a number, an array, a quoted string) is a text line
- [ ] A line longer than 16 KiB is cut at a UTF-8 boundary at or below that length and marked as cut; a cut JSON line is stored as text (it no longer parses)
- [ ] No guessing in text lines: `ERROR something` has no level (ADR-0067 §3)
- [ ] A JSON line with no message key gets an empty message and keeps all its keys in fields

## Implementation Notes
- Use `serde_json` (already a dependency) with `Map<String, Value>`; do not define a struct per logger
- Fixture lines from `tracing-subscriber` JSON (with and without `flatten_event`), `aphid-server` with `APHID_LOG_FORMAT=json`, pino, bunyan, zap, logrus, Python `structlog`, and nginx's default text access line
- Keep the mapping tables as constants in one place; the logging guide (docs task of this epic) lists the same keys

## Edge Cases
- Duplicate keys in one object: `serde_json` keeps the last; document it in a test
- `level` present but `null`
- Invalid UTF-8 in the line: replaced lossily before parsing (the follower decides bytes → `str`)
- A trailing `\r` from CRLF output is stripped

## Testing
- Table-driven unit tests over the fixtures; a `proptest` that any input parses without panicking and that a cut line is at most 16 KiB and valid UTF-8
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§3)
- `src/runtime/podman.rs` (`ContainerLog`, how frames become lines today)