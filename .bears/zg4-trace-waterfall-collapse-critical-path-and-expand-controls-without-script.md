---
id: zg4
title: "Trace waterfall: collapse, critical path and expand controls without script"
status: done
priority: P3
created: "2026-11-12T19:37:41.658293029Z"
updated: "2026-11-13T22:41:26.370828855Z"
tags:
  - web
  - observability
depends_on:
  - aum
parent: qnh
attempts: 1
---

## Summary
The waterfall's controls from the mockup: collapsing a span's subtree, "expand all", "collapse to services" and the critical-path highlight, all under the CSP without a new script.

## Acceptance Criteria
- [ ] Each span with children has a toggle with `aria-expanded` and an accessible name ("Collapse documents.info"); a collapsed span shows `+N` hidden descendants
- [ ] Collapse state is in the URL (`?collapsed=<ids>`), so links and reloads keep it; the toggles are links (HTMX may swap the waterfall in place, but it works without)
- [ ] "expand all" clears it; "collapse to services" collapses every server span that has children and a parent (the mockup's rule)
- [ ] "critical path" is a switch (`role="switch"`, `aria-checked`) that dims every span not on the critical path; it works with CSS only (a checkbox and `:has()`), falling back to a `?crit=on` link
- [ ] The critical path and the collapsed set come from the trace type (task `txt`), not from templates

## Implementation Notes
- Check `:has()` against the browsers the admin UI supports; the `?crit=on` server path is the fallback either way

## Testing
- Handler tests: collapse parameter round trip, collapse-to-services on the mockup's `t1`, critical path set, `+N` counts
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- `docs/design/Telemetry.dc.html` (waterfall controls), ADR-0054, ADR-0060
- The waterfall task `aum`