---
id: vd3
title: CI integration guide and example release workflow
status: done
priority: P2
created: "2026-11-07T14:45:17.349199545Z"
updated: "2026-11-08T02:40:17.046314994Z"
tags:
  - docs
depends_on:
  - xhn
  - j7u
parent: hxh
attempts: 1
---

## Summary
Document how an app repo deploys to hoster.

## Acceptance Criteria
- [ ] `docs/deploying-from-ci.md`: build+push with docker/build-push-action, capture digest, `permissions: id-token: write`, request the OIDC token with the hoster audience, POST with `Authorization: Bearer`; how to find `repository_id` and write the stack `[deploy]` rule; recommended GitHub environment protection rules (ADR-0047)
- [ ] `examples/github-actions/release.yml` working example matching the final payload format
- [ ] Verified once against a real hoster instance (noted in PR)

## Testing
- Claims example from the doc is used in the webhook tests
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- spec.md §5.2