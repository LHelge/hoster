---
id: x58
title: "Logs page: the page scrolls far past the log box"
status: done
priority: P2
created: "2026-11-14T21:53:40.980359577Z"
updated: "2026-11-14T22:07:17.526712291Z"
tags:
  - web
  - ui
---

## Summary
Owner report of 2026-11-14: on the logs pages the outer page can be scrolled far below the log box, as if the page counted the log's own scrolled-away rows.

Cause: `.visually-hidden` is `position: absolute`, and records inside the scrolling `.log` box contain such spans ("no level" in `log_record.html`, "stderr: " in `log_record.html` and `log_lines.html`). `.log` and `.log-view` are not positioned, so those spans' containing block is the initial one: they sit at their static position deep inside the log's scrolled content, escape the `overflow: auto`/`hidden` clipping, and stretch the document.

## Acceptance Criteria
- [x] `.log` gets `position: relative` (assets/hoster.css, Logs section), so absolutely positioned descendants are clipped by and scroll with the log box
- [ ] Checked in a browser on the stack logs tab, Telemetry → Logs and a container's live log: the page ends at the footer (not done when the fix landed; check on the next deploy)
- [x] Other scroll containers with `.visually-hidden` content: `.help-scroll` and `.live-check .live-problems` (apply page) got `position: relative` too; `.table-scroll` and `.sidebar` were already positioned

## References
- `assets/hoster.css` (`.visually-hidden`, `.log-view`, `.log`), `templates/log_record.html`, `templates/log_lines.html`, ADR-0054