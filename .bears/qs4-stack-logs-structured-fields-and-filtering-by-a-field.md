---
id: qs4
title: "Stack logs: structured fields and filtering by a field"
status: open
priority: P3
created: "2026-11-12T13:26:21.008380610Z"
updated: "2026-11-12T13:26:21.008380610Z"
tags:
  - web
  - observability
depends_on:
  - yn7
parent: psq
---

## Summary
A record that came from a JSON line carries its remaining keys as fields (ADR-0067 §3). Show them on demand in the stack logs tab and let a field value become a filter, so "every line of this request" or "every line from this module" is one click.

## Acceptance Criteria
- [ ] A record with fields expands in place (a `<details>` row, no script) to a key/value list: nested objects flattened to dotted keys (`fields.user_id`, `span.name`), values in the mono font, long values wrapped not truncated
- [ ] The expanded row also shows what the list view leaves out: full timestamp, stream, container generation, and whether the line was cut at the length cap
- [ ] Each value links to the same view with `?field=<key>=<value>` added; active field filters show as removable chips in the toolbar. Several can be active and all must match
- [ ] A field filter is an exact match on the flattened key, done in the store's scan beside grep, under the same scan cap and "stopped early" notice
- [ ] `tracing`'s `target` is shown in the collapsed row when present (dimmed, after the level), since it is what the owner's apps always have
- [ ] Records without fields have no expander
- [ ] Field keys and values are escaped everywhere, including in the links

## Implementation Notes
- Flattening belongs on the record type the store returns (a method yielding `(key, value)` pairs), not in the template
- Parse the fields JSON once per listed record; do not parse during the scan unless a field filter is active
- If the grep task has landed, a grep match inside the fields opens that record's `<details>`

## Edge Cases
- Arrays (`spans` in `tracing` output is an array of objects): index in the key (`spans.0.name`)
- A value that is itself a long JSON string or contains newlines
- Keys containing `=` or `.`: the filter parameter must round-trip (encode the key, split on the first unencoded `=`)
- Fields JSON that no longer parses (it was valid when stored): show it raw

## Testing
- Unit tests for flattening and for the filter parameter round trip (property test with `proptest` for odd keys)
- Handler tests: expander only where fields exist, filter chips, escaping
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§3, §4, §8), ADR-0054
- `src/web/stacks/logs.rs`, `docs/design/StackLogs.dc.html`