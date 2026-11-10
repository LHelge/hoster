---
id: "5v3"
title: Edit the current revision from the stack page
status: open
priority: P2
created: "2026-11-10T17:51:50.736778368Z"
updated: "2026-11-10T17:51:50.736778368Z"
tags:
  - web
  - ui
parent: emu
---

## Summary
Today "Apply a new revision" on a stack page opens an empty editor, so the user has to paste the whole file again. Add an **Edit** action that opens the apply page prefilled with the active revision's TOML exactly as it was applied (`revisions.toml`, comments included). If another revision became active in the meantime, the preview warns about it.

## Acceptance Criteria
- [ ] The stack page's apply link becomes **Edit stack file** (primary), next to the existing way of applying a fresh file. Both go to `/apply`. Edit is `/apply?stack=<name>`, and an unknown stack is a 404.
- [ ] The editor is prefilled with the active revision's stored TOML byte for byte, and the heading or crumb names the stack ("Edit web", `~/stacks/web/edit`). Without an active revision, it is the empty New stack page.
- [ ] The form carries the base revision number in a hidden field. Check and apply (preview and confirm) compare it with the stack's active revision. If they differ, the preview shows a warning naming both revisions ("Revision 7 became active after you opened the editor at revision 6"), and the diff is against the *current* active revision, as today. The warning doesn't block applying.
- [ ] Renaming the stack (`name = …`) in the editor behaves as today: the preview shows it as a new stack. The base revision of another stack never triggers the warning.
- [ ] Works with and without editor.js, and with live diagnostics once that lands.

## Testing
- Web tests: prefill is byte-exact (comments, trailing newline); unknown stack gives a 404; no active revision falls back to the New page; a base-revision mismatch warns in the preview; a matching base doesn't; a renamed stack doesn't.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0021, ADR-0027 (stack files applied through the UI), ADR-0049, ADR-0054
- `src/web/stacks/apply.rs`, `templates/apply.html`, `templates/stack.html` (the apply link), `src/store/` (revisions)