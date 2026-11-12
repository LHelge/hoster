---
id: zyf
title: "Telemetry → Traces tab: trace list"
status: open
priority: P2
created: "2026-11-12T19:37:09.325570794Z"
updated: "2026-11-12T19:37:56.274186380Z"
tags:
  - web
  - observability
depends_on:
  - txt
  - ckc
parent: qnh
---

## Summary
Add the Traces tab to the Telemetry page (the Logs tab is task `ckc` of epic `psq`): the trace list of `docs/design/Telemetry.dc.html` with the page's stack selector and range presets, and selecting a trace by URL.

## Acceptance Criteria
- [ ] `/telemetry/traces` becomes the page's first tab and `/telemetry` redirects to it; the tab bar shows both tabs with counts for the range
- [ ] Columns time, root span, stack, services (chips in the categorical service palette of `docs/design/README.md`), spans, duration (with a bar scaled to the slowest listed), status; "app not instrumented" where only the proxy's spans exist
- [ ] Sort select newest, slowest, errors first; controls for status and a minimum duration (the query box stays grep on span names, OQ-43); every option is a GET parameter
- [ ] `?trace=<id>` selects a trace; the selected row is marked and the waterfall (task after this) renders below the list on the same page, as in the mockup. A trace id that does not exist (expired after 3 days) says so
- [ ] Range presets 5m, 15m, 1h, 6h, 24h (spans are kept 3 days; 7d is not offered here)
- [ ] The header's receiver line shows the receiver's state and spans/logs per second over the last minute, without the mockup's gRPC port (OQ-42 answer 4)
- [ ] Service colours, status tones and the error ring are CSS classes; no inline styles (ADR-0054)

## Testing
- Handler tests: list, sorts, filters, selection, expired trace, proxy-only label
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- `docs/design/Telemetry.dc.html` (traces tab), `docs/design/README.md` (Telemetry notes), ADR-0054, OQ-42, OQ-43
- The Telemetry page task `ckc`, `src/web/mod.rs`