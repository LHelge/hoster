---
id: pks
title: Dashboard "Recent events" card with live append over SSE
status: open
priority: P2
created: "2026-11-09T14:05:24.837685697Z"
updated: "2026-11-09T14:05:24.837685697Z"
tags:
  - web
  - ui
  - observability
depends_on:
  - nbj
parent: "4qj"
---

## Summary
The design's Recent events card (`docs/design/Main.dc.html`). It introduces the reusable event-row partial that the per-stack feed and Events page reuse.

## Acceptance Criteria
- [ ] An `event_row` partial: time (relative, absolute in `title`), tone dot from `Severity`, message, mono meta line, linked to its subject (deployment, stack/service, certificate) when there is one
- [ ] A card on the dashboard with the newest ~10 events and a "view all" link to the Events page (link target added by the Events page task; until then link to nothing or hide it)
- [ ] Live: `UiEvent::Event` prepends the row over the dashboard's existing `GET /events` SSE stream, trimming to the card's length. A lagged receiver re-renders the card from the store
- [ ] Empty state when there are no events
- [ ] Styles follow the design system (ADR-0054); no inline styles (CSP); icons through the `Icon` enum (ADR-0053)

## Implementation Notes
- `src/web/dashboard.rs` (`OverviewEvents` already maps `UiEvent`s to partial refreshes), `templates/dashboard.html`, `templates/dashboard_overview.html`
- Decide whether the card is part of the overview partial or its own SSE event name; a separate named event avoids re-rendering the whole overview on each new event

## Testing
- Template render tests for each severity and with/without subject link; SSE test that an event appends a row
- `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0055, ADR-0021, ADR-0053, ADR-0054
- `docs/design/Main.dc.html`, `src/web/dashboard.rs`