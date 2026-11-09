---
id: "4ft"
title: Per-stack event feed and the Events page; document the event log
status: open
priority: P2
created: "2026-11-09T14:05:41.564105124Z"
updated: "2026-11-09T14:05:41.564105124Z"
tags:
  - web
  - ui
  - observability
  - docs
depends_on:
  - pks
parent: "4qj"
---

## Summary
The two remaining views of ADR-0055 §5, and the docs.

## Acceptance Criteria
- [ ] Stack overview: a feed of the stack's recent events (same `event_row` partial), live over the stack page's SSE, with a link to the Events page filtered to that stack
- [ ] Events page (`/events` path is taken by the dashboard SSE: pick e.g. `/activity` or `/events/all`, and keep URLs consistent with the existing route constants): every event newest-first, cursor-paginated ("older" link, works without JS), filters for kind group (deploys / containers / certificates / operator & system), minimum severity and stack, as query parameters
- [ ] A removed stack's events stay filterable by name
- [ ] The dashboard card's "view all" link points here
- [ ] Sidebar nav entry if the design has one; otherwise reachable from the dashboard card and stack pages
- [ ] Docs: architecture.md gets a short section on the event log (table, producers, retention, the `UiEvent` hand-off); `spec.md` mentions the feed; the user-facing wiki page that covers the dashboard/observability (if any in `site/content/wiki/`) mentions it. `aphid build` in `site/` passes

## Implementation Notes
- `src/web/stacks/overview.rs`, `src/web/pages.rs`, `src/web/mod.rs` (routes), `templates/`
- Reuse the store's cursor query from the store task; no offset paging

## Testing
- Handler tests: filters, paging boundary, unknown/removed stack, invalid query params → 400 or ignored consistently with other pages
- `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0055, ADR-0054
- `docs/design/StackDetail.dc.html`, `src/web/stacks/overview.rs`