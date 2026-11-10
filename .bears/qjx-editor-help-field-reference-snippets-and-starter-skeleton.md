---
id: qjx
title: "Editor help: field reference, snippets and starter skeleton"
status: open
priority: P2
created: "2026-11-09T17:22:16.248321729Z"
updated: "2026-11-10T17:52:08.816528469Z"
tags:
  - web
  - ui
depends_on:
  - zcu
  - "9za"
parent: emu
---

## Summary
Make the web editor self-explanatory without a form builder and without app templates (ADR-0018). Rescoped 2026-11-10: diagnostics moved to x97 (live diagnostics), and highlighting and the gutter to 9za (editor script). This task keeps the reference, the snippets and the skeleton.

## Acceptance Criteria
- [ ] **Field reference:** a collapsible reference in the apply page's aside, rendered on the server from the JSON Schema (`src/stack/schema.rs`, built once, not per request). It lists every field with its type, default and description, grouped by table (top level, service, route, health, deploy, files, …). It uses `<details>`, so no JS is needed. A test fails if a schema field is missing from the reference.
- [ ] **Snippets:** generic, never app-specific, for a service, a route, a volume, a health check (HTTP and exec), `[deploy]` and `files`, plus the fields of later epics as they land (M13 runtime options and limits, M14 `auto_update`; their tasks add them). With editor.js, a button inserts a snippet at the cursor, keeping undo, with the same insert helper as 9za. Without JS, each snippet is a readable `<pre>` to copy. Every snippet parses when inserted into the skeleton (a test).
- [ ] **Starter skeleton:** an empty New stack editor (not Edit, see 5v3) starts with a commented minimal stack and the `#:schema` line pointing at the public schema URL. The skeleton parses (a test). Placeholders are in comments, so applying it unchanged fails with a helpful diagnostic, not silently.
- [ ] Matches the design system (ADR-0054) and works on narrow screens.

## Testing
- Askama and web tests in `src/web/stacks/tests.rs`: reference completeness against the schema, snippets parse, skeleton parses, and the no-JS fallback renders.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `node --test assets/` (if the insert helper changes).

## References
- ADR-0018, ADR-0021, ADR-0054, ADR-0060
- `docs/design/NewStack.dc.html` (reference mockup)
- `src/stack/schema.rs`, `src/web/stacks/apply.rs`, `templates/apply.html`