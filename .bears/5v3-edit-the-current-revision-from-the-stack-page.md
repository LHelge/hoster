---
id: "5v3"
title: Edit the current revision from the stack page
status: done
priority: P2
created: "2026-11-10T17:51:50.736778368Z"
updated: "2026-11-10T19:40:17.598314224Z"
tags:
  - web
  - ui
parent: emu
attempts: 1
---

## Summary
Today "Apply a new revision" on a stack page opens an empty editor, so the user has to paste the whole file again. Add an **Edit** action that opens the apply page prefilled with the active revision's TOML exactly as it was applied (`revisions.toml`, comments included). If another revision became active in the meantime, the preview warns about it.

## Acceptance Criteria
- [x] The stack page's apply link becomes **Edit stack file** (primary), next to the existing way of applying a fresh file ("Apply a new file"). Both go to `/apply`. Edit is `/apply?stack=<name>`, and an unknown stack is a 404.
- [x] The editor is prefilled with the active revision's stored TOML byte for byte, and the heading or crumb names the stack ("Edit web", `~/stacks/web/edit`). Plain `/apply` (or an empty `?stack=`) is the empty New stack page. A stack without an active revision is not listed by `status()`, so it is a 404 like an unknown one.
- [x] The form carries the base revision (`base_stack`, `base_revision` hidden fields, in both the multipart and the urlencoded form). The preview compares it with the stack's active revision. If they differ, the Pre-flight card shows a warning naming both revisions ("Revision 7 became active after you opened the editor at revision 6"), and the diff is against the *current* active revision, as today. A stack removed meanwhile warns that applying creates it again. The warning doesn't block applying. The confirm (`POST /apply`) does not compare again: the preview is where the admin reviews the diff.
- [x] Renaming the stack (`name = …`) in the editor behaves as today: the preview shows it as a new stack. The base revision of another stack never triggers the warning.
- [x] Works with and without editor.js, and with live diagnostics: the edit page runs the live check on load (`hx-trigger="load, input changed delay:500ms"`); the New stack page does not, since its skeleton is incomplete.

## Testing
- Web tests: prefill is byte-exact (leading blank line, comments, quotes, trailing newline); unknown stack gives a 404; empty `?stack=` is the New page; a base-revision mismatch warns in the preview (urlencoded and multipart); a matching base doesn't; a renamed stack doesn't; a removed stack says so.
- UI preview states `apply-edit` and `apply-edit-stale`, checked in headless Chromium.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0021, ADR-0027 (stack files applied through the UI), ADR-0049, ADR-0054
- `src/web/stacks/apply.rs`, `templates/apply.html`, `templates/stack.html` (the apply link), `src/store/` (revisions)