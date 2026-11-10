---
id: x97
title: Live diagnostics while editing a stack file
status: done
priority: P2
created: "2026-11-10T17:51:50.726988586Z"
updated: "2026-11-10T19:01:14.944737498Z"
tags:
  - web
  - ui
  - stack
depends_on:
  - "9za"
parent: emu
attempts: 1
---

## Summary
While the user types, hoster checks the stack file and shows the diagnostics next to the editor. The script marks the affected lines in the gutter and jumps to them on click. Validation stays on the server (ADR-0060): this is the Rust parser and pre-flight, not a JS reimplementation.

## Acceptance Criteria
- [x] **Endpoint** `POST /apply/check` (authenticated, CSRF-checked like `/apply/preview`) returns an HTML fragment of diagnostics. It runs every *local* check:
  - parse and validation (`Stack::parse`);
  - route collisions with other stacks;
  - the reserved admin host;
  - that referenced secrets exist;
  - the cgroup controller and podman version pre-flight, where they exist.

  Nothing goes over the network: registry tag resolution and the DNS check stay on Check (`/apply/preview`). Share the code with the preview path and `hoster stack check`; don't fork the checks.
- [x] **Trigger:** the textarea posts with HTMX on `input` after a pause of about 500 ms (`hx-trigger` with `changed delay:500ms`, `hx-sync` replacing a request still in flight). It works without editor.js too.
- [x] **Fragment:** a list of diagnostics with severity, `line:column`, field path and message, in the same wording as the UI's failed-apply page and `hoster stack check`. A clean file shows a short "No problems found" state. Each item carries `data-line` and `data-column`.
- [x] **Script:** after the fragment swaps in (`htmx:afterSwap` via `addEventListener`), editor.js marks those lines in the gutter (error and warning styles) and makes each item select its line and column in the textarea when clicked. Without JS the list is still readable.
- [x] **Layout:** diagnostics sit between the editor and its footer, or in the aside, per `docs/design/NewStack.dc.html`. The plan card from Check is not replaced. Works on narrow screens, and `aria-live="polite"` doesn't announce every keystroke (it announces only when the count changes).
- [x] **Cost:** the check holds no orchestrator lock longer than a read and starts no deployment. A file over the apply size limit is rejected with the same message as Check.

## Testing
- Web tests in `src/web/stacks/tests.rs`: unauthenticated or no CSRF is rejected; a parse error comes back with `data-line`; a collision with another stack is reported; a missing secret is reported; and no registry call is made (the fake registry records no request).
- `node --test`: the helper that maps a diagnostic's line and column to a textarea selection offset (tabs, multi-byte characters).
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `node --test assets/`.

## References
- ADR-0060, ADR-0021, ADR-0054
- `src/web/stacks/apply.rs`, `templates/apply.html`, `templates/apply_failed.html`, `src/stack/diagnostic.rs`, `src/stack/check.rs`, `src/orchestrator/mod.rs` (`prepare`)
- `docs/design/NewStack.dc.html` (reference only)

## Result
- `Orchestrator::check_file` (next to `preview`, sharing `check_routes` and `check_engine` with `prepare`) returns a `CheckReport` of errors and warnings, each located. It never resolves a tag (the test counts resolver calls). There is no cgroup pre-flight yet; M13's xxb adds its controller check here too.
- **Referenced secrets that are not set are warnings, not errors.** Apply doesn't reject them (only `Validating` does), and a new stack's secrets can only be set once it exists.
- `FieldPath::locate(source)` finds any path's key in the source through `toml::de::DeTable` spans. It gives route collisions (via `Collision::path`), a removal in progress, the podman version check and unset secrets a line, where before only parse errors had one.
- `POST /apply/check` reads only the `toml` field: the textarea posts urlencoded (`hx-encoding`), so a chosen upload is never sent on each pause. It always answers 200, with an empty-editor or "could not check" panel where needed.
- Panel under the editor (`#apply-check`): a `role="status"` summary swapped with `innerMorph`, so an unchanged result changes no DOM and isn't announced again. Each located problem has `data-line`/`data-severity` and a `#stack-toml` link with `data-column`. editor.js follows the panel with a MutationObserver: it marks the gutter, and a link click puts the caret on the problem (`offsetAt`, counting characters like the server).
- UI preview: the live check's `hx-post` is stripped like the streams; every apply state shows its panel, plus `states/apply-typing.html`.
- Verified in headless Chromium (with the preview server standing in for the endpoint): load trigger, one request per burst of typing, gutter mark on the right line, click to caret, marks cleared when fixed, no CSP violations.
- Not done: the Check (preview) failure card's problems don't mark the gutter.

