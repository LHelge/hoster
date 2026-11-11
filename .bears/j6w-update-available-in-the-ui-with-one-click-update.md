---
id: j6w
title: "\"Update available\" in the UI with one-click update"
status: open
priority: P2
created: "2026-11-09T17:23:25.608421705Z"
updated: "2026-11-09T17:23:25.608421705Z"
tags:
  - web
  - orchestrator
depends_on:
  - jcw
parent: "6z3"
---

## Summary
Surface stored update candidates, and let the operator apply them through the existing apply preview / diff flow.

## Acceptance Criteria
- [ ] An orchestrator method, e.g. `update_preview(stack, selection)`, builds a new revision from the current one. It rewrites the selected services' tags in the TOML text with an edit that preserves formatting and comments (`toml_edit`, added with `cargo add` if not already a dependency) keeping the operator's style (ADR-0061 §9: `repo:tag` gets only the new tag, with the digest stored in the revision; `repo:tag@sha256:…` gets both). It returns the usual `ApplyPreview`. Applying it uses the normal apply path (ADR-0049). Model this on `reresolve_preview` / `reresolve` in `src/orchestrator/mod.rs`.
- [ ] Stack overview: per service, "postgres 16.4 → 16.6 (patch)", "tag `8` moved", and a major available shown separately. Buttons: "Update" per service and "Update all" (patch + minor + moved, never major unless chosen explicitly). Both lead to the preview.
- [ ] Version groups per the ADR are offered and applied together.
- [ ] The stacks list gets a badge; the dashboard warnings say "N updates available"; Settings or System shows the last check time with "Check now".
- [ ] Deployment history shows the update as an apply with the tag change in its diff.

## Testing
- Orchestrator tests: tag rewrite preserves comments and formatting, a group is applied together, a stale candidate (revision changed since the check) is refused.
- Web tests for the overview, list, dashboard and preview.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0061; ADR-0025, ADR-0049, ADR-0054
- `src/orchestrator/mod.rs` (`reresolve_preview`, `reresolve`), `src/web/stacks/apply.rs`, `src/web/stacks/overview.rs`, `src/web/stacks/list.rs`, `src/web/dashboard.rs`