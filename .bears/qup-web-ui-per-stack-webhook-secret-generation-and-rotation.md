---
id: qup
title: "Web UI: per-stack webhook secret generation and rotation"
status: cancelled
priority: P3
created: "2026-11-07T14:45:17.334238337Z"
updated: "2026-11-07T19:59:12.442492768Z"
tags:
  - web
  - security
depends_on:
  - xhn
  - "3g2"
  - j7u
parent: hxh
---

## Summary
Manage webhook secrets from the stack page.

## Acceptance Criteria
- [ ] Generate secret (shown once), rotate (old secret valid for a short overlap), revoke
- [ ] Secrets stored in the encrypted secret store (ADR-0029)

## Testing
- Handler tests; rotation overlap test
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0042