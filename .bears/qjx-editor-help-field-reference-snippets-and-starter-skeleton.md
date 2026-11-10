---
id: qjx
title: "Editor help: field reference, snippets and starter skeleton"
status: done
priority: P2
created: "2026-11-09T17:22:16.248321729Z"
updated: "2026-11-10T19:11:48.793114847Z"
tags:
  - web
  - ui
depends_on:
  - zcu
  - "9za"
parent: emu
attempts: 1
---

## Summary
Make the web editor self-explanatory without a form builder and without app templates (ADR-0018). Rescoped 2026-11-10: diagnostics moved to x97 (live diagnostics), and highlighting and the gutter to 9za (editor script). This task keeps the reference, the snippets and the skeleton.

## Acceptance Criteria
- [x] **Field reference:** a collapsible reference in the apply page's aside, rendered on the server from the JSON Schema (`src/stack/schema.rs`, built once, not per request). It lists every field with its type, default and description, grouped by table (top level, service, route, health, deploy, files, …). It uses `<details>`, so no JS is needed. A test fails if a schema field is missing from the reference.
- [x] **Snippets:** generic, never app-specific, for a service, a route, a volume, a health check (HTTP and exec), `[deploy]` and `files`, plus the fields of later epics as they land (M13 runtime options and limits, M14 `auto_update`; their tasks add them). With editor.js, a button inserts a snippet at the cursor, keeping undo, with the same insert helper as 9za. Without JS, each snippet is a readable `<pre>` to copy. Every snippet parses when inserted into the skeleton (a test).
- [x] **Starter skeleton:** an empty New stack editor (not Edit, see 5v3) starts with a commented minimal stack and the `#:schema` line pointing at the public schema URL. The skeleton parses (a test). Placeholders are in comments, so applying it unchanged fails with a helpful diagnostic, not silently.
- [x] Matches the design system (ADR-0054) and works on narrow screens.

## Testing
- Askama and web tests in `src/web/stacks/tests.rs`: reference completeness against the schema, snippets parse, skeleton parses, and the no-JS fallback renders.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `node --test assets/` (if the insert helper changes).

## References
- ADR-0018, ADR-0021, ADR-0054, ADR-0060
- `docs/design/NewStack.dc.html` (reference mockup)
- `src/stack/schema.rs`, `src/web/stacks/apply.rs`, `templates/apply.html`

## Result
- `src/web/stacks/editor_help.rs`: `EditorHelp` (built once) holds the reference, the snippets and the skeleton.
  - The reference walks the JSON Schema from its root, following `$ref`s. Each nested table, array of tables (`[[…]]`) and table of tables (`services.<service>`, `files."<path>"`) becomes a section headed by its TOML header. A field M13 or M14 adds shows up without a change.
  - Fields list required ones first, then the rest. Each shows its kind (with enums as `"start-first" | "stop-first"`), its default as TOML (empty `[]`/`{}` left out) and its description, with `code` spans kept.
  - A table's description shows only its first paragraph; the top-level one is otherwise a page long.
  - Tests check that every schema field is in the reference and that the headers are right.
- **Snippets**: service, route, volume, HTTP and command health checks, config file, `[deploy]`. Each parses when added to the filled-in skeleton (a test).
  - Each is a `<pre>` to copy. editor.js shows an Insert button that puts it after the caret's line, set off by blank lines, and selects it. Undo takes it back.
  - Per-service snippets, written for `services.web`, are moved to the service whose table the caret is in (`serviceAt`, `retarget`, tested with node).
- **Skeleton**: the `#:schema` line, then `version`, a blank `name` and a `web` service with a blank `image`, explained in comment lines. Every line fits in 60 characters (a test).
  - Applied unchanged it fails with errors on exactly those two lines (a test).
  - The live check no longer fires on page load, so the skeleton doesn't open with two red errors; 5v3 can add `load` for a prefilled editor.
- Both help sections are collapsible `<details>` cards in the aside (now labelled "Plan and help"), closed by default. Their content scrolls inside (60vh).
- Verified in headless Chromium:
  - Insert puts a route on the caret's service (`api`), selected and highlighted; undo removes it.
  - `[deploy]` is appended at the end.
  - Light and dark both render; no horizontal scroll at 375 px; no CSP violations.

