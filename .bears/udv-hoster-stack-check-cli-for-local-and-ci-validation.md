---
id: udv
title: "`hoster stack check` CLI for local and CI validation"
status: open
priority: P3
created: "2026-11-09T17:21:18.120246756Z"
updated: "2026-11-09T17:21:18.120246756Z"
tags:
  - cli
  - stack
parent: emu
---

## Summary
`hoster stack check <file>...` parses and validates stack files offline with `Stack::parse`. It prints every diagnostic with file:line:column and the field path (the same text the UI shows) and exits non-zero on errors. Warnings print but exit 0, unless `--deny-warnings` is given. It needs no running hoster, config or podman, so app repositories can run it in CI.

## Acceptance Criteria
- [ ] Subcommand in the existing CLI (`src/main.rs` clap tree), behaviour on the `Stack` / `Diagnostic` types (no free functions), with human output and `--format json`.
- [ ] Checks that need other stacks or the server config (route collisions, reserved admin host, cgroup pre-flight) are explicitly skipped, with a note in the output.
- [ ] deploying-from-ci.md shows a CI step that runs it (via the release binary).

## Testing
- CLI tests on valid, invalid and warning-only files; snapshot of the output format.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- `src/stack/diagnostic.rs`, `src/main.rs`
- `site/content/wiki/deploying-from-ci.md`