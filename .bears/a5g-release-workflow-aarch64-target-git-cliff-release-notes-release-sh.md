---
id: a5g
title: "Release workflow: aarch64 target, git-cliff release notes, release.sh"
status: open
priority: P1
created: "2026-11-08T08:25:15.945638084Z"
updated: "2026-11-08T08:25:15.945638084Z"
tags:
  - ci
  - ops
parent: tbf
---

## Summary
Extend `.github/workflows/release.yml` so a `vX.Y.Z` tag on main publishes static musl binaries for x86_64 and aarch64, with release notes generated from Conventional Commits by git-cliff. Add `scripts/release.sh` to cut a release.

## Acceptance Criteria
- [ ] Build matrix: `x86_64-unknown-linux-musl` on `ubuntu-latest`, `aarch64-unknown-linux-musl` natively on GitHub's ARM runner (`ubuntu-24.04-arm`); fmt/clippy/test run on both; `scripts/check-release-binary.sh` passes for both; `rust-toolchain.toml` lists both targets
- [ ] Release files: `hoster-x86_64-unknown-linux-musl`, `hoster-aarch64-unknown-linux-musl`, one combined `SHA256SUMS`, plus the deploy/ files (and `install.sh` once it exists — package it if present, like deploy/)
- [ ] `cliff.toml` at the repo root groups commits by type (Features, Fixes, Performance, Documentation, Other; `chore(release)` commits skipped) for the range previous tag..this tag; the release body is git-cliff's output (replace `--generate-notes`)
- [ ] Tag on a commit not reachable from `main` fails the workflow
- [ ] Existing rules kept: tag must equal `Cargo.toml` version; pre-release suffix → GitHub pre-release; workflow_dispatch builds but never publishes
- [ ] `scripts/release.sh X.Y.Z`: refuses unless on a clean, up-to-date `main` and X.Y.Z is valid SemVer greater than the current version; bumps `Cargo.toml` + `Cargo.lock`, commits `chore(release): vX.Y.Z` (with the repo's trailer conventions), creates an annotated tag, prints the git-cliff preview, and pushes branch + tag only after a confirmation prompt (`--yes` to skip)
- [ ] README 'Development' section documents cutting a release and previewing notes (`git cliff --unreleased`)

## Implementation Notes
- Use the official git-cliff GitHub action or install the binary; pin versions.
- No aws-lc-rs check changes needed beyond running it per target.
- The workflow has never run on GitHub (no remote yet): validate with actionlint and shellcheck locally, and note in the summary that a real tag run is still pending.

## Testing
- actionlint + shellcheck clean
- `git cliff` produces sensible notes locally for the current history
- `scripts/release.sh` tested in a throwaway clone (dry path, refusal cases)

## References
- ADR-0052 (Releases section)
- ADR-0001, ADR-0037
- .github/workflows/release.yml, scripts/check-release-binary.sh