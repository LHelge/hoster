---
id: "9za"
title: "Editor script: highlighted TOML overlay, gutter and keys"
status: open
priority: P2
created: "2026-11-10T17:51:29.840028226Z"
updated: "2026-11-10T17:51:29.840028226Z"
tags:
  - web
  - ui
depends_on:
  - "6n2"
parent: emu
---

## Summary
Write `assets/editor.js` per ADR-0060. It turns the stack textarea on the apply page into a highlighted TOML editor with a line-number gutter and editing keys. Without JS the page is unchanged.

## Acceptance Criteria
- [ ] **Asset:** `editor.js` is embedded and served hashed through `src/web/assets.rs` (like `hoster.css`), and only pages with the editor load it (`<script type="module" src="…">`, CSP unchanged). The asset tests cover its hashed URL.
- [ ] **Opt-in by markup:** the script enhances every `textarea[data-editor="toml"]`. Nothing else on the page changes.
- [ ] **Overlay:** a `<pre aria-hidden="true">` behind the textarea, with matching font, size, line height, padding, border, `tab-size` and `white-space: pre`. The textarea's text is transparent and its caret visible. Scrolling stays in sync both ways, a trailing newline is handled, and highlighting is redrawn on `input`.
- [ ] **Tokenizer:** an exported pure function `tokenize(text) -> [{kind, text}]` covering:
  - comments, `[table]` and `[[array]]` headers, bare, quoted and dotted keys, `=`;
  - basic and literal strings, plus multi-line `"""` and `'''` (escapes inside basic strings);
  - integers and floats (with `_`, hex, octal and binary, `inf`, `nan`), booleans, and offset or local dates and times;
  - arrays and inline tables.

  Invalid input degrades to plain text and never throws. Joining the token texts gives back the input exactly.
- [ ] **Gutter:** line numbers that follow the textarea's scrolling, plus an API (`markLines(lines, severity)`) the live-diagnostics task uses.
- [ ] **Keys:**
  - Tab inserts two spaces, and Shift+Tab outdents the current line or selection.
  - Escape and then Tab moves focus on, so keyboard users are never trapped.
  - Enter keeps the current line's indent.
  - These edits keep native undo (`execCommand('insertText')` with a `setRangeText` fallback; record which one works where).
- [ ] **Theme:** token classes in `hoster.css` with Catppuccin Mocha and Latte colours following `prefers-color-scheme` (ADR-0054). Contrast is readable in both, and narrow screens work (no horizontal page scroll, the editor scrolls inside itself).
- [ ] **CI:** a step runs `node --test assets/` (the `editor.test.mjs` tests) with no npm install. README or CONTRIBUTING notes how to run them locally.

## Testing
- `assets/editor.test.mjs`:
  - tokenizer cases for each token kind;
  - multi-line strings across lines;
  - round-trip (joined tokens equal the input) on the stack-format.md examples and a malformed file;
  - the indent helpers as pure functions.
- Rust web tests: the apply page loads the hashed script and marks the textarea with `data-editor="toml"`. With the script absent, the form still posts as before.
- Manual browser check in Firefox and Chromium, light and dark: typing, paste, undo, IME, scrolling a long file, and narrow width.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `node --test assets/`.

## References
- ADR-0060, ADR-0054, ADR-0053 (hashed assets)
- `src/web/assets.rs`, `assets/hoster.css`, `templates/apply.html`, `.github/workflows/ci.yml`
- `site/content/wiki/stack-format.md` (examples for the round-trip test)