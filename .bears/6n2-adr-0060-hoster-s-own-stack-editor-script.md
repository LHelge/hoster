---
id: "6n2"
title: "ADR-0060: hoster's own stack editor script"
status: open
priority: P2
created: "2026-11-10T17:51:13.984156840Z"
updated: "2026-11-10T17:51:13.984156840Z"
tags:
  - decision
  - docs
  - adr
  - web
parent: emu
---

## Summary
Record the editor decisions the owner made while planning M15 (2026-11-10) as ADR-0060, amending ADR-0021 (the admin UI stack) and ADR-0054 (the design system, CSP). It is the first JavaScript hoster writes itself, so the ADR sets the rules for that too.

## Decisions to record (owner, 2026-11-10)
1. **One hoster-owned script, no dependencies:** `assets/editor.js`, embedded and served under a content-hashed name like `hoster.css` (`src/web/assets.rs`). It is not vendored, so `assets/VENDORED.md` is unchanged. No npm, no `package.json`, no bundler, no build step.
2. **Progressive enhancement:** the page stays a plain `<textarea>` form that works without JS (submit, upload, HTMX check and preview). The script only adds things on top.
3. **Overlay editor:** a highlighted `<pre>` behind a transparent textarea, with the caret, selection, IME, undo and form value staying native. It adds a line-number gutter, keys (Tab inserts two spaces, Escape then Tab leaves the field, Enter keeps the indent), and line markers for diagnostics. A plain textarea with no highlighting is not good enough.
4. **Own TOML tokenizer:** a pure function that colours the text and does not validate it. The Rust parser stays the only authority, reached through the server (live diagnostics). Tokens map to CSS classes in `hoster.css` with Catppuccin Mocha/Latte colours.
5. **CSP unchanged:** `script-src 'self'`, no inline scripts, no `eval`, no `hx-on:*`, no `style=` attributes. Highlight spans are built from DOM nodes and text, never `innerHTML` from user text. Event hooks between HTMX and the script go through `addEventListener` and `data-*` attributes.
6. **Tests:** the tokenizer and other pure helpers are an ES module tested by `node --test` (`assets/editor.test.mjs`) in a CI step, with no npm. Locally the tests run only where node is installed. The Rust tests keep checking the HTML hooks.
7. **Validation stays on the server:** live diagnostics are a debounced HTMX post running every *local* check (no registry or DNS calls). Network checks stay behind the Check button.

## Alternatives to record as rejected
- **CodeMirror 6:** no official single-file build. Bundling it at vendoring time makes every upgrade a rebuild, and the bundle is the largest asset (~300 KB+).
- **CodeMirror 5:** in maintenance mode only.
- **Ace:** large, and a strict CSP needs its CSS loaded separately.
- **Monaco:** far too large, and needs its own loader.
- **Prism plus our own overlay:** it only replaces the ~150-line tokenizer, at the cost of a vendored file, its default theme and upgrade tracking.
- **contenteditable editors (CodeJar):** they lose the native textarea's form value, undo and accessibility.
- **WASM build of the stack parser:**
  - It needs a wasm toolchain or a committed `.wasm` with a drift check.
  - Stack parsing would have to move into its own workspace crate.
  - The CSP would need `'wasm-unsafe-eval'`.
  - Server-state checks would still need the server.
  - It is deferred to a proposed spike rather than rejected for good.
- **No editor (server-side help only)** and **schema-driven key completion in JS:** not now. Completion could be a later follow-up on top of this script.

## Acceptance Criteria
- [ ] `docs/adr/0060-*.md` written in the template's shape (Context, Decision, Alternatives, Consequences), Status Accepted, dated 2026-11-10.
- [ ] `docs/adr/README.md` gets the 0060 row, and the 0021 and 0054 rows note the amendment ("own editor script per 0060").
- [ ] `assets/VENDORED.md`'s CSP notes mention that `editor.js` is hoster's own and follows the same rules.
- [ ] The Context section explains why the editor matters (stack files are hand-written, compose is deferred, ADR-0018 forbids templates).

## References
- ADR-0018, ADR-0021, ADR-0053 (hashed assets), ADR-0054
- `assets/VENDORED.md` (CSP notes), `src/web/assets.rs`, `src/web/mod.rs` (`CSP`)
- `templates/apply.html`, `docs/design/NewStack.dc.html`