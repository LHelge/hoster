---
id: emu
title: "M15: Stack authoring guidance"
type: epic
status: open
priority: P2
created: "2026-11-09T17:20:07.837547939Z"
updated: "2026-11-09T17:20:07.837547939Z"
tags:
  - docs
  - web
  - stack
  - third-party
---

## Scope

Make hand-writing a `*.stack.toml` for any containerized app easy enough that neither a graphical stack builder nor compose support is needed for now (owner 2026-11-09: compose deferred). Branch: `epic/<id>-stack-authoring`.

- A JSON Schema for the stack format, generated from the parser's types and kept in sync by a test. It gives completion, hover docs and validation in VS Code / Taplo / any editor with TOML schema support.
- Better help in the web editor: diagnostics that point at the line, and a field reference drawn from the schema.
- A "Running third-party images" guide, including a compose → stack TOML translation table and a full worked example.
- `hoster stack check <file>` for validating a stack file locally or in CI.

## Acceptance Criteria

- [ ] The schema is published (docs site and hoster itself) and covers every field, with descriptions. A test fails when the parser and the schema diverge.
- [ ] The New stack / apply editor shows diagnostics by line and offers the field reference.
- [ ] The guide exists on the docs site, linked from overview and stack-format, and `aphid build` passes.
- [ ] `hoster stack check` exits non-zero with the same diagnostics as the UI.

## References

- ADR-0018 (no app-specific templates: guidance stays generic), ADR-0021, ADR-0054
- `site/content/wiki/stack-format.md`, `site/content/wiki/overview.md`
- `src/stack/` (parser and diagnostics)