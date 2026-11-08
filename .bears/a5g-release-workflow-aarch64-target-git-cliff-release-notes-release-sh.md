---
id: a5g
title: "Release workflow: aarch64 target, git-cliff release notes, release.sh"
status: done
priority: P1
created: "2026-11-08T08:25:15.945638084Z"
updated: "2026-11-08T09:31:39.520615505Z"
tags:
  - ci
  - ops
parent: tbf
attempts: 1
---

## Summary
Extend `.github/workflows/release.yml` so a `vX.Y.Z` tag on main publishes static musl binaries for x86_64 and aarch64, with release notes generated from Conventional Commits by git-cliff. Add `scripts/release.sh` to cut a release through a PR (main is branch-protected: no direct pushes, linear history, required CI checks).

## Acceptance Criteria
- [ ] Build matrix: `x86_64-unknown-linux-musl` on `ubuntu-latest`, `aarch64-unknown-linux-musl` natively on GitHub's ARM runner (`ubuntu-24.04-arm`); fmt/clippy/test run on both; `scripts/check-release-binary.sh` passes for both; `rust-toolchain.toml` lists both targets
- [ ] Release files: `hoster-x86_64-unknown-linux-musl`, `hoster-aarch64-unknown-linux-musl`, one combined `SHA256SUMS`, plus the deploy/ files (and `install.sh` once it exists — package it if present, like deploy/)
- [ ] `cliff.toml` at the repo root groups commits by type (Features, Fixes, Performance, Documentation, Other; `chore(release)` commits skipped) for the range previous tag..this tag; the release body is git-cliff's output (replace `--generate-notes`)
- [ ] Tag on a commit not reachable from `main` fails the workflow
- [ ] Existing rules kept: tag must equal `Cargo.toml` version; pre-release suffix → GitHub pre-release; workflow_dispatch builds but never publishes
- [ ] `scripts/release.sh prepare X.Y.Z`: refuses unless on a clean `main` that matches `origin/main` and X.Y.Z is valid SemVer greater than the current version; creates branch `release/vX.Y.Z`, bumps `Cargo.toml` + `Cargo.lock`, commits `chore(release): vX.Y.Z` (repo trailer conventions), pushes and opens a PR with `gh pr create` whose body is the git-cliff preview
- [ ] `scripts/release.sh tag X.Y.Z` (after the PR is merged): refuses unless local `main` matches `origin/main`, its HEAD is the `chore(release): vX.Y.Z` commit and `Cargo.toml` says X.Y.Z; creates an annotated tag on that commit and pushes only the tag, after a confirmation prompt (`--yes` to skip). GitHub's rebase merge rewrites SHAs, so the tag must be created after merging, never on the PR branch
- [ ] README 'Development' section documents both steps and previewing notes (`git cliff --unreleased`)

## Implementation Notes
- Use the official git-cliff GitHub action or install the binary; pin versions.
- No aws-lc-rs check changes needed beyond running it per target.
- Required status check names on main are the CI job names; don't rename CI jobs without updating branch protection.
- Validate with actionlint and shellcheck locally; the first real tag run happens when v0.1.0 is cut.

## Testing
- actionlint + shellcheck clean
- `git cliff` produces sensible notes locally for the current history
- `scripts/release.sh` tested in a throwaway clone with a bare local remote (refusal cases; `gh` calls stubbed)

## References
- ADR-0052 (Releases section and its 2026-11-08 correction)
- ADR-0001, ADR-0037
- .github/workflows/release.yml, scripts/check-release-binary.sh