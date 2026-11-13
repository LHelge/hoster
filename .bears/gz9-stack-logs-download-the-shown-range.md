---
id: gz9
title: "Stack logs: download the shown range"
status: done
priority: P3
created: "2026-11-12T13:26:42.586128686Z"
updated: "2026-11-13T13:16:22.490354052Z"
tags:
  - web
  - observability
depends_on:
  - btj
parent: psq
attempts: 1
---

## Summary
A **Download** button on the stack logs tab saves what the current filters select, over the whole time range and not only the page shown, as a file.

**Scope (2026-11-12).** The owner chose one log view with two scopes: the stack tab and the Telemetry page's Logs tab (`ckc`) are the same view, the stack tab with its stack fixed. Download is mounted on the stack tab only for now; it is built on the shared view's query, so offering it on the Telemetry page later is a mount, not new code.

## Acceptance Criteria
- [ ] `GET /stacks/{name}/logs/download` takes the tab's parameters (service, since, level, grep, field filters where they exist) and answers with `Content-Disposition: attachment` and a file name of stack, range and format (`blog-2026-11-12T10-00Z-1h.log`)
- [ ] `format=text` (the default): one line per record, `<RFC 3339 time> <service> <level or -> <message>`. `format=jsonl`: one JSON object per record with time, stack, service, generation, stream, level, message and fields
- [ ] The body is streamed page by page from the store; memory use does not grow with the size of the download
- [ ] A cap on records per download (a constant). When it is hit the file ends with a line saying it was cut and from which time, and the button's hint states the cap
- [ ] The route requires the admin session like every other page; it is a GET with no side effects, so no CSRF token
- [ ] The button keeps the current filters and offers both formats

## Implementation Notes
- An Axum streaming body (`Body::from_stream`) over the store's id-paged query; the grep scan cap applies per page, not per download, so a filtered download can cover the full range
- Text format: messages can contain newlines; escape them (`\n`) so one record is one line. Say so in the first line of the file only if that keeps it greppable; otherwise document it in the logging guide
- File name pieces are derived from validated values only (stack names are already restricted); never echo a raw parameter into the header

## Edge Cases
- No matching records: an empty file with status 200, not an error page
- The client disconnects midway: the stream stops and stops querying
- An invalid regular expression: 400 with the same message the tab shows

## Testing
- Handler tests: headers, both formats, the cap line, newline escaping, a download larger than one page
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§8), ADR-0031 (sessions, CSRF)
- `src/web/logs.rs` (the shared view: `LogParams`, `LogsView::plan`, `Links`, `LogMount`), `src/web/logs/download.rs`, `templates/logs_view.html`
- `src/web/stacks/logs.rs`, `src/web/mod.rs` (routes), `src/store/logs/query.rs` (`LogStore::records`, `Page`)
- `site/content/wiki/logging.md` (the user-facing description)