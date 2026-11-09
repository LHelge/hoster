---
id: qjx
title: Editor help on the New stack and apply pages
status: open
priority: P2
created: "2026-11-09T17:22:16.248321729Z"
updated: "2026-11-09T17:22:16.248321729Z"
tags:
  - web
  - ui
depends_on:
  - zcu
parent: emu
---

## Summary
Make the web editor for stack files self-explanatory, without a form builder and without app templates (ADR-0018).

## Acceptance Criteria
- [ ] Diagnostics from a failed validate are shown next to the textarea with line:column and field path. Each one links to (selects) the offending line in the editor. No inline scripts: use vendored HTMX plus the existing hashed JS assets, within the current CSP (ADR-0054).
- [ ] A collapsible field reference in the aside, rendered server-side from the JSON Schema (zcu): every field, its type, default and description, grouped by table (top level, service, route, health, deploy, …).
- [ ] Generic snippets the user can insert or copy: service, route, volume, health check, `[deploy]`, and the new fields as their epics land. Each snippet is generic, never app-specific.
- [ ] An empty New stack editor starts with a commented minimal skeleton and the `#:schema` line.
- [ ] Matches the design system (ADR-0054) and works on narrow screens.

## Testing
- Askama template tests / web tests in `src/web/stacks/tests.rs` for the diagnostics rendering and the reference.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0018, ADR-0021, ADR-0054
- `docs/design/NewStack.dc.html` (reference mockup)
- `src/web/stacks/apply.rs`, `templates/apply.html`, `templates/apply_failed.html`, `templates/apply_preview.html`