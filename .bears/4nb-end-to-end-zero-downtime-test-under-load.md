---
id: "4nb"
title: End-to-end zero-downtime test under load
status: done
priority: P1
created: "2026-11-07T14:45:17.315113504Z"
updated: "2026-11-08T02:10:24.864023355Z"
tags:
  - test
depends_on:
  - shg
parent: mdd
attempts: 1
---

## Summary
Prove ADR-0017 with a gated end-to-end test on real podman.

## Acceptance Criteria
- [ ] Test harness starts hoster (plain HTTP or TLS with test certs), deploys a test image v1, runs continuous requests (keep-alive and new connections, plus one long streaming request), deploys v2
- [ ] Zero non-2xx responses and zero connection errors during the deploy; responses switch from v1 to v2
- [ ] A broken v3 (fails health) never receives traffic

## Implementation Notes
- Small test image built locally in the test (or public image with a version env var)

## Testing
- Runs in CI on a runner with rootless podman when available; otherwise documented manual run
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0017