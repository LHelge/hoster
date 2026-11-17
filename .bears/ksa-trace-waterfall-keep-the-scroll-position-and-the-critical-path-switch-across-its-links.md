---
id: ksa
title: "Trace waterfall: keep the scroll position and the critical-path switch across its links"
status: done
priority: P3
created: "2026-11-17T20:11:02.440134353Z"
updated: "2026-11-17T21:25:42.555971706Z"
tags:
  - web
  - observability
attempts: 1
---

## Summary
Found while implementing epic `qnh` (tasks `zg4`, `jpq`) on 2026-11-17. Every link of the waterfall (select a span, close the details, collapse, expand, "collapse to services") is a full page load, and the waterfall sits below the trace list and the scatter, so each click lands at the top of the page. For the same reason the CSS-only critical-path switch (`zg4`: a checkbox and `:has()`) is reset by every such link; only `?crit=on` in the URL lasts, and in browsers with `:has()` the UI offers no way to set it.

## Acceptance Criteria
- [ ] Following a waterfall link keeps the waterfall in view (an HTMX swap of `section.trace-waterfall`, or a fragment on its links; a row anchor needs care because one trace can hold two spans with the same id)
- [ ] The critical-path highlight survives selecting a span and collapsing (for example the switch drawn as the `?crit=on` link everywhere, with the checkbox only as an instant preview, or the swap leaving the checkbox alone)
- [ ] Still no inline styles and no new script (ADR-0054, ADR-0060); works without HTMX

## References
- ADR-0054, ADR-0060, ADR-0021
- `templates/telemetry_traces.html`, `src/web/telemetry/traces.rs`, `src/web/telemetry/traces/waterfall.rs`, `assets/hoster.css` (`.wf-crit-*`)