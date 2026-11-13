---
id: "9yu"
title: Composite action actions/stack-check, used on site/docs.stack.toml in CI
status: done
priority: P2
created: "2026-11-13T10:04:47.575242Z"
updated: "2026-11-13T10:10:18.876292Z"
tags:
  - ci
  - stack
depends_on:
  - "45e"
parent: "8kz"
---

## Summary
A composite action at `actions/stack-check/` that replaces the hand-written download in the CI guide's §2.6.

## Acceptance Criteria
- [ ] `actions/stack-check/action.yml` with inputs `files` (whitespace-separated paths or globs), `version` (a release tag such as `v0.7.0`, default `latest`; pin the server's), `deny-warnings` (default `false`)
- [ ] Downloads `hoster-<arch>-unknown-linux-musl` for `runner.arch` (X64, ARM64) and verifies it against the release's `SHA256SUMS`; fails clearly on a non-Linux runner
- [ ] Prints the text report and adds `::error`/`::warning` annotations with file, line and column from `--format json`; exits as `hoster stack check` does
- [ ] CI checks `site/docs.stack.toml` with `uses: ./actions/stack-check` (the latest release is what the server runs)

## References
- `site/content/wiki/deploying-from-ci.md` §2.6, `src/stack/check.rs` (JSON shape), `.github/workflows/ci.yml`