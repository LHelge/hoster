---
id: zcu
title: JSON Schema for stack files, generated and kept in sync
status: open
priority: P2
created: "2026-11-09T17:21:18.111817244Z"
updated: "2026-11-09T17:21:18.111817244Z"
tags:
  - stack
  - docs
parent: emu
---

## Summary
Publish a JSON Schema (draft 2020-12) for `*.stack.toml`, so editors with TOML schema support (Taplo / VS Code "Even Better TOML", JetBrains) give completion, hover docs and validation. The schema must not drift from the parser.

## Acceptance Criteria
- [ ] The schema covers every field of the stack format with `description`s (from doc comments), enums (`update`, `expect_status` patterns), name / host / prefix patterns and duration / byte-size string patterns. `additionalProperties: false` mirrors `deny_unknown_fields`.
- [ ] It is generated from the raw serde types in `src/stack/raw.rs` (e.g. `schemars` derive, added with `cargo add`), or, as a fallback, a hand-written schema checked by a test that walks the raw types. Pick generation unless span wrappers make it unworkable.
- [ ] Committed as `site/static/stack.schema.json` (or wherever aphid serves static files) and published at `https://docs.hoster.lhelge.se/stack.schema.json`. hoster also serves it at `/schema/stack-v1.json` on the admin host.
- [ ] A test regenerates it and fails with a "run `cargo run -- stack schema > …`" hint when the committed file differs. That way later epics (M11–M14) that add fields can't forget it.
- [ ] stack-format.md explains `#:schema https://docs.hoster.lhelge.se/stack.schema.json` as the first line, and the Taplo / VS Code setup.

## Implementation Notes
- `Spanned<T>` wrappers in `raw.rs` need a transparent schema (`schemars(with = …)` or a manual `JsonSchema` impl).
- Semantic rules that a schema can't express (route collisions, cycles) stay parser-only; say so in the description.

## Testing
- Schema snapshot test; a validation test that the worked example in stack-format.md validates against the schema (`jsonschema` dev-dependency).
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- `src/stack/raw.rs`, `src/stack/mod.rs`
- `site/content/wiki/stack-format.md` §1, §4