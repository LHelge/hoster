---
id: "6qz"
title: Notification mail templates in the admin UI's style
status: open
priority: P3
created: "2026-11-09T23:22:59.540441772Z"
updated: "2026-11-09T23:22:59.540441772Z"
tags:
  - notify
  - ui
depends_on:
  - rja
parent: c5s
---

## Summary
Askama templates that turn an event into a mail that is recognisably hoster: the same colours, the event-row layout (tone, readable line, mono meta line) and wording as the UI. Mail clients ignore most of `assets/hoster.css`, so the mail has its own inline-styled template rather than reusing the stylesheet.

## Acceptance Criteria
- [ ] A typed notification built from an `Event` (`From`/`TryFrom`, not a free function) that yields the message parts of the provider trait: subject, plain text and HTML.
- [ ] Subject is `[<instance name>] <readable line>` (ADR-0058 §10). The instance name is passed in; this task does not read it from the store.
- [ ] HTML template: table layout, inline styles only, colours taken from the light (Latte) design tokens in `assets/hoster.css`, system sans and monospace font stacks, the tone shown as a coloured cell (no SVG, no remote images, no web fonts), one button linking to the stack or the Events page on the admin host.
- [ ] A plain-text template with the same content.
- [ ] Variants: a problem, a recovery, the "N more events" summary, and the test message.
- [ ] Every interpolated value is escaped by Askama; event messages can contain image names and error text from outside.
- [ ] The token values copied into the mail template are noted next to the copies in `site/theme/`, so a token change has one list of places to update (see `assets/VENDORED.md` or the nearest equivalent note).

## Implementation Notes
- Templates live in `templates/` beside the others (for example `mail_event.html`, `mail_event.txt`). `templates/event_row.html` is the layout to mirror.
- Design for light mode and let clients invert; don't add dark-mode media queries in the first version.
- `src/web/ui_preview.rs` renders UI pieces for review. Add the mail variants there so they can be looked at in a browser without sending anything.

## Testing
- Render tests per variant: subject, the link target, escaping of a hostile message, and that the HTML has no `<style>` block, `<svg>` or remote URL other than the admin host link.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the tests pass.

## References
- ADR-0058, ADR-0054, ADR-0055
- `templates/event_row.html`, `assets/hoster.css`, `src/web/ui_preview.rs`, `src/store/types.rs` (`Event`, `Severity`)