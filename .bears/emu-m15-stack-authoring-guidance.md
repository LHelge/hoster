---
id: emu
title: "M15: Stack authoring guidance"
type: epic
status: done
priority: P2
created: "2026-11-09T17:20:07.837547939Z"
updated: "2026-11-10T20:29:23.941746657Z"
tags:
  - docs
  - web
  - stack
  - third-party
---

## Scope

Make hand-writing a `*.stack.toml` for any containerized app easy enough that neither a graphical stack builder nor compose support is needed for now (owner 2026-11-09: compose deferred). Branch: `epic/emu-stack-authoring`.

Done and already on main (landed with M12): the JSON Schema (zcu) and `hoster stack check` (udv).

Planned with the owner on 2026-11-10:
- **ADR-0060 (6n2):** hoster's first own JavaScript, a dependency-free `assets/editor.js`. It is progressive enhancement with the CSP unchanged, tested by `node --test` in CI. CodeMirror, Prism, Ace and WASM were rejected; WASM is a proposed spike, dyb.
- **Editor script (9za):** a highlighted TOML overlay on the textarea with its own tokenizer, a line-number gutter and editing keys.
- **Live diagnostics (x97):** a debounced HTMX check that runs every local check on the server, with gutter markers and click-to-line.
- **Editor help (qjx):** a field reference from the schema, generic snippets and a starter skeleton.
- **Edit the current revision (5v3):** prefilled from the active revision, with a stale-base warning.
- **Guide (k2v):** "Running third-party images", written now. M13 and M14 add their sections.

## Acceptance Criteria

- [x] The schema is published (docs site and hoster itself) and covers every field, with descriptions. A test fails when the parser and the schema diverge.
- [x] `hoster stack check` exits non-zero with the same diagnostics as the UI.
- [x] ADR-0060 records the editor decisions.
- [x] The apply editor highlights TOML, shows line numbers and has editing keys, and the page still works without JS.
- [x] Diagnostics appear while typing, by line, with markers in the gutter, from the same checks as Check (minus network).
- [x] The editor offers the field reference, snippets and a starter skeleton.
- [x] A stack's active file can be edited from its page, with a warning when the base revision is stale.
- [x] The guide exists on the docs site, linked from overview and stack-format, and `aphid build` passes.

## References

- ADR-0018 (no app-specific templates: guidance stays generic), ADR-0021, ADR-0054, ADR-0060
- `site/content/wiki/stack-format.md`, `site/content/wiki/overview.md`
- `src/stack/` (parser and diagnostics), `src/web/stacks/apply.rs`, `templates/apply.html`, `src/web/assets.rs`