---
id: btj
title: "Stack logs: level filter and grep"
status: open
priority: P2
created: "2026-11-12T13:26:20.968219900Z"
updated: "2026-11-12T13:26:20.968219900Z"
tags:
  - web
  - observability
depends_on:
  - yn7
parent: psq
---

## Summary
Add the two filters of `docs/design/StackLogs.dc.html` to the shared log view (both scopes, so the Telemetry Logs tab `ckc` gets them too): **level** (all, error, warn+, info, debug: the chips of both mockups together) and **grep** (a substring, or a regular expression with the `.*` toggle). Both run in the log store's query, which scans the indexed range in Rust (ADR-0067 §8).

## Acceptance Criteria
- [ ] `?level=warn` shows `warn` and `error`, `?level=error` shows `error`; `?level=info` and `?level=debug` show that level alone, as the Telemetry mockup's chips do (`trace` records are shown with debug); the default shows everything
- [ ] Records without a level (text lines, ADR-0067 §3) are shown only under "all". The toolbar says so when a level filter hides them, with the count hidden if the store can give it cheaply
- [ ] `?q=` matches a case-insensitive substring of the message and of the fields JSON; with `?regex=on` it is a regular expression. Matches in the message are wrapped in `<mark>`
- [ ] An invalid regular expression re-renders the tab with the error beside the field and no records; it is never a 500
- [ ] The regular expression is compiled with a size limit, and a query stops after the store's scan cap. When it stops early the page says how far back it searched and offers "search older"
- [ ] The footer shows the counts of the design for what is listed: lines shown, errors, warnings
- [ ] Filters combine with the service chip, the time range and paging, and every link keeps them

## Implementation Notes
- The filter belongs to the store's query type; the handler only parses GET parameters into it (`TryFrom<&Query>` as `LogOptions` does in `src/web/containers.rs`)
- Use the `regex` crate if it is already in the tree (`cargo tree -i regex`); otherwise `cargo add regex`. It guarantees linear-time matching, so a hostile pattern cannot stall the scan; the scan cap bounds the rest
- Highlighting is done on the server on escaped text; never mark inside an HTML entity

## Edge Cases
- An empty `q` is no filter; a `q` of only spaces is a literal search
- A match only in the fields still lists the record, with the fields opened if the fields task has landed
- Level and grep with zero results: an empty state that names the active filters and links "clear filters"

## Testing
- Store-level tests for level and text matching, the scan cap and the "stopped early" signal
- Handler tests for parameter parsing, invalid patterns, highlighting and escaping (a message containing `<script>` and `&amp;`)
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§3, §8), ADR-0054
- `docs/design/StackLogs.dc.html`, `docs/design/Telemetry.dc.html` (severity chips), `src/web/stacks/logs.rs`, `src/web/containers.rs` (`LogQuery`, `LogOptions`)