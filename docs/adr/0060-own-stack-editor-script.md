# ADR-0060: The stack editor is hoster's own dependency-free script over a plain textarea; validation stays on the server

- Status: Accepted (amends ADR-0021 and ADR-0054)
- Date: 2026-11-10

## Context

Stack files are written by hand. Compose support is deferred (owner, 2026-11-09), and ADR-0018
rules out app templates, so the editor on the apply page is where most stacks are written. Today
that editor is a bare `<textarea>`: no highlighting, no line numbers, and diagnostics appear only
after **Check**. The stack JSON Schema (`/schema/stack-v1.json`) and diagnostics with a line,
column and field path already exist (M15, `src/stack/schema.rs`, `src/stack/diagnostic.rs`).

ADR-0021 chose HTMX so the UI "needs no JS build", and ADR-0054 kept the CSP at `script-src
'self'` with no inline scripts, no `eval` and no `style=` attributes. The only scripts so far are
vendored HTMX and its SSE extension. A better editor is the first JavaScript hoster writes
itself, so this ADR also sets the rules for that.

## Decision

The owner made these choices on 2026-11-10 (epic emu):

1. **One script of hoster's own, with no dependencies.** `assets/editor.js` is embedded and
   served under a content-hashed name like `hoster.css` (`src/web/assets.rs`), and loaded as a
   `<script type="module">` only on pages with an editor. It is not vendored, so
   `assets/VENDORED.md` does not list it. There is no npm, `package.json`, bundler or build step.
2. **Progressive enhancement.** The markup is a working form without the script: the textarea
   posts, uploads and runs the HTMX check and preview as before. The script enhances each
   `textarea[data-editor="toml"]`, and nothing else.
3. **An overlay, not a replacement.** A highlighted `<pre aria-hidden="true">` sits behind the
   textarea, whose text is transparent. Caret, selection, IME, spell-check settings, undo and the
   form value stay the browser's. The script adds a line-number gutter, line markers for
   diagnostics, and keys: Tab inserts two spaces, Shift+Tab outdents, Escape then Tab leaves the
   field, and Enter keeps the indent.
4. **An own TOML tokenizer.** It is a pure function that colours text and never validates it.
   Broken TOML degrades to plain text, and joining the tokens always gives back the input. Token
   kinds map to classes in `hoster.css` with Catppuccin Mocha and Latte colours (ADR-0054).
5. **Validation stays on the server.** Diagnostics while typing come from a debounced HTMX post
   that runs every *local* check (parse, validation, collisions, reserved host, secrets,
   pre-flight) through the same code as Check and `hoster stack check`. Checks that go over the
   network (registry tags, DNS) run only on Check. The Rust parser is the only authority on what
   a stack file means.
6. **The CSP is unchanged.** The script builds highlight spans from DOM nodes and text, never
   `innerHTML` from user input. It talks to HTMX through `addEventListener` and `data-*`
   attributes, never `hx-on:*`. Sizes and positions come from classes, or from properties the
   script sets on elements (CSSOM), which `style-src 'self'` allows; there are no `style=`
   attributes in markup.
7. **Tests with `node --test`.** The script's pure parts (tokenizer, indent and position helpers)
   are exported and tested by `assets/editor.test.mjs` in a CI step with no install. The DOM
   wiring runs only when a `document` exists, so node can import the file. Rust tests keep
   checking the HTML hooks.

## Alternatives considered

- **CodeMirror 6**: a full editor (lint gutter, completion, search), but there is no official
  single-file build. It would be bundled once at vendoring time and rebuilt for every upgrade,
  and it would be the largest asset by far.
- **CodeMirror 5**: one prebuilt file with a TOML mode, but in maintenance mode only.
- **Ace**: prebuilt with a TOML mode, but large, and a strict CSP needs its styles loaded
  separately from the script.
- **Monaco**: far larger again, with its own loader.
- **Prism with our own overlay**: replaces only the tokenizer, about a third of the script, at
  the cost of a vendored file, its theme to override and upgrades to track for one language.
- **A contenteditable editor (CodeJar and similar)**: small, but the form value, undo and
  accessibility become the script's job instead of the browser's.
- **The stack parser compiled to WASM**: reuses `Stack::parse` and the diagnostics in the
  browser, with a tokenizer in Rust. But it needs a wasm toolchain or a committed `.wasm`, a
  workspace crate for `src/stack/` without the server's dependencies, and `'wasm-unsafe-eval'`
  in the CSP. Checks against server state would still need the server. Deferred, not rejected:
  a spike (Bears dyb) measures the cost.
- **No editor, server-side help only**: no JavaScript at all, but no highlighting or line
  numbers, which are most of what makes a long file readable.
- **Key completion from the schema in the script**: useful, and the most script of any feature.
  Not now; the field reference and snippets come first, and local editors already complete keys
  from the `#:schema` line.

## Consequences

- hoster now owns JavaScript. Every later script follows the same rules: hoster's own and
  dependency-free unless an ADR vendors something, progressive enhancement, CSP unchanged, pure
  parts tested by `node --test`.
- Contributors need node only to run the script's tests locally; building and `cargo test` do not.
- A module served under a hashed name cannot import a sibling by its plain name, so each script
  is one self-contained file.
- The tokenizer knows TOML, not the stack format, so new stack fields (M13, M14) never touch it.
  They reach the editor through the schema (field reference) and snippets.
- Live diagnostics cost one small request per pause in typing. The check takes no lock longer
  than a read and never starts a deployment.
- If the WASM spike says yes, a new ADR supersedes decisions 4 and 5 and changes the CSP.
