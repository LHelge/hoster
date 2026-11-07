---
id: "4ah"
title: "Release build and packaging: static binary, hoster release workflow, install artifact"
status: open
priority: P2
created: "2026-11-07T21:12:34.648755575Z"
updated: "2026-11-07T21:12:34.648755575Z"
tags:
  - ci
  - ops
depends_on:
  - mnu
parent: "8xd"
---

## Summary
ADR-0001 says "one statically-linked-as-practical binary" and docs/install.md (task `meu`) will tell operators to install it, but no task produces it. Verify the static build is feasible with this dependency set (sqlx bundles libsqlite3, ring has C/asm, pingora) and ship a release workflow.

## Acceptance Criteria
- [ ] A release profile builds `hoster` for `x86_64-unknown-linux-musl` (or, if musl is impractical for a dependency, a documented glibc baseline) with `cargo build --release`; the result is checked with `ldd`/`file` in CI
- [ ] `.github/workflows/release.yml`: on a `v*` tag, build, run tests, attach the binary + `sha256sum` + the systemd unit/sysctl/example config from `deploy/` to a GitHub release
- [ ] `docs/install.md` (task `meu`) points at the release artifact
- [ ] Binary size and build time recorded in the PR

## Implementation Notes
- Try `cross` or a `musl-tools` runner first; fall back to a glibc build with `-C target-feature=+crt-static` only if a dependency refuses musl
- No dependency on Pingora/bollard being present yet: the workflow is wired on the skeleton and keeps working as crates are added

## Edge Cases & Considerations
- `ring`'s assembly on musl is fine; `aws-lc-rs` must stay out of the tree (ADR-0037, `cargo tree -i aws-lc-rs`)

## Testing
- The workflow runs on a tag in a fork or with `workflow_dispatch` before merge
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0001, ADR-0037