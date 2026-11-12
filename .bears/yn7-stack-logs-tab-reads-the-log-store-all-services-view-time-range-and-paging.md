---
id: yn7
title: "Stack logs tab reads the log store: all-services view, time range and paging"
status: open
priority: P2
created: "2026-11-12T13:25:46.343095158Z"
updated: "2026-11-12T13:25:46.343095158Z"
tags:
  - web
  - observability
depends_on:
  - f8z
parent: psq
---

## Summary
`GET /stacks/{name}/logs` shows the stack's persisted records from the log store (ADR-0067) instead of one container's podman stream: every service merged in time order, with a service column, a time range and paging back through history. This is the base the other tasks of the epic add filters, follow, fields, download and sources to.

**Scope (2026-11-12).** The owner chose one log view with two scopes: the stack tab and the Telemetry page's Logs tab (`ckc`) are the same view, the stack tab with its stack fixed. This task builds that view as a module of its own and mounts it on the stack tab first.

## Acceptance Criteria
- [ ] The view, its parameter parsing and its templates are one module (for example `src/web/logs/`) parameterised by a `LogScope` (`Stack(name)` or `All { stack: Option<…> }`); the stack tab mounts it with `Stack`, and the Telemetry Logs tab (`ckc`) will mount it with `All` without copying handlers or templates
- [ ] hoster's own records that name the stack (ADR-0068) are listed under the service `hoster`, with its own chip
- [ ] The tab lists the stack's records oldest to newest (newest at the bottom): time, service, level, message. Order is by time, then record id
- [ ] Service chips as in `docs/design/StackLogs.dc.html`: **all** (the default) plus one per service that has records in the range or is in the current revision; `?service=` picks one. The service column is colour-coded with a fixed palette of CSS classes assigned by service order (no inline styles, ADR-0054)
- [ ] Records of replaced and removed containers are shown, with the deployment generation visible where a service has more than one in the range
- [ ] `since` presets 5m, 15m, 1h, 6h, 24h and 7d (shared with the Telemetry Logs tab) plus the free text the tab accepts today (a duration or a UTC time); default 1h. All options are plain GET parameters and every link keeps the others, as `LogView` does now
- [ ] A page shows at most a fixed number of records (a constant); "older" and "newer" links page by record id, not by offset
- [ ] When the store's oldest record is newer than the start of the asked range (size cap or a young server), the page says from when its data starts
- [ ] A text line (no level) and a `stderr` line are distinguishable, as stdout/stderr are today
- [ ] The marker record for dropped lines (ADR-0067 §7) renders as a notice row, not as a log line
- [ ] Each service chip row links to the live container log page (`/containers/{name}/logs`), which stays as it is (ADR-0067 §10). If the store cannot be read, the tab says so and offers those links instead of failing
- [ ] An unknown stack is still a 404; a stack with no records shows an empty state that explains JSON logging in one line and links the logging guide
- [ ] The module doc of `src/web/stacks/logs.rs`, the Logs sentence in `site/content/wiki/overview.md` and the web UI part of `docs/architecture.md` describe the new tab

## Implementation Notes
- `src/web/stacks/logs.rs` owns the tab; today it picks one container and embeds `LogView` from `src/web/containers.rs`. Keep `Containers::log_page` and `log_stream` untouched
- Read through the query API the log store epic (`f8z`) provides; if it lacks something this page needs (paging by id, oldest-record time), add it there, on the store type, not in the handler
- Templates: `templates/log_panel.html`, `log_lines.html`, `log_options.html` are shared with the container page; give the stack tab its own templates rather than branching the shared ones
- Times are rendered as elsewhere in the UI; the row keeps the full timestamp in a `title` or `<time datetime>`

## Edge Cases
- A removed stack's name that still has records: 404 as today (the tab belongs to an existing stack)
- A service renamed between revisions appears under both names
- Thousands of records in the same millisecond page correctly (id tiebreak)

## Testing
- Handler tests with a seeded store, as `src/web/stacks/tests.rs` does for other tabs: merged order, service filter, paging links, the "data starts at" notice, empty state, store failure
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§4, §7, §8, §10), ADR-0068, ADR-0054, ADR-0021
- `docs/design/StackLogs.dc.html`, `docs/design/Telemetry.dc.html` (logs tab), `src/web/stacks/logs.rs`, `src/web/containers.rs`, `templates/log_*.html`