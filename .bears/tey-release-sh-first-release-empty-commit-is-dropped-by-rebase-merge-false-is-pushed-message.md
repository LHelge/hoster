---
id: tey
title: "release.sh: first-release empty commit is dropped by rebase merge; false \"is pushed\" message"
status: open
priority: P2
created: "2026-11-08T11:20:11.503513558Z"
updated: "2026-11-08T11:20:11.503513558Z"
tags:
  - ci
  - ops
---

## Summary
Two bugs found cutting v0.1.0.

1. `prepare 0.1.0` with Cargo.toml already at 0.1.0 creates an empty `chore(release): v0.1.0` commit. GitHub's rebase merge drops empty commits, so PR #5 merged as a no-op and `tag 0.1.0` then refuses ("the tip of main is ..., not chore(release)"). v0.1.0 was tagged by hand on e6487ed.
2. When `git push` of the release branch fails (SSH timeout to github.com), the EXIT trap still prints "release/vX.Y.Z is pushed but has no PR", because `pushed_branch` is set before the push. Nothing was pushed.

## Acceptance Criteria
- [ ] `tag` no longer depends on the tip being a `chore(release)` commit that may not survive the merge: e.g. accept the tip when Cargo.toml says X.Y.Z, vX.Y.Z is untagged, and either the tip is the release commit or the release PR for vX.Y.Z is merged (`gh pr view release/vX.Y.Z --json state`) — or drop the empty-commit path and make the first release a direct `tag` without `prepare`
- [ ] `pushed_branch` is set only after `git push` succeeds; a failed push gets the "nothing was pushed" cleanup path (switch back to main, delete local branch)
- [ ] scripts/test-release.sh covers both cases; README 'Cutting a release' updated

## References
- ADR-0052 (Correction)
- scripts/release.sh, scripts/test-release.sh