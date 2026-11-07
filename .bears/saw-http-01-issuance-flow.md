---
id: saw
title: HTTP-01 issuance flow
status: open
priority: P1
created: "2026-11-07T14:45:17.270749593Z"
updated: "2026-11-07T14:45:17.270749593Z"
tags:
  - tls
  - acme
depends_on:
  - hez
  - nra
parent: "52q"
---

## Summary
Issue a certificate for one host via HTTP-01 served by the :80 service (architecture §7.3).

## Acceptance Criteria
- [ ] issue(host): new_order → for each authz pick Http01 → insert token→key_authorization into ChallengeMap → set_ready → poll_ready → finalize → poll_certificate → store via CertStore → remove challenge
- [ ] Errors typed (validation failed with CA detail, rate limited, timeout)
- [ ] One certificate per hostname (ADR-0035)

## Testing
- Gated integration test: Pebble + hoster :80 on a test port with Pebble's HTTP-01 port override; resulting cert served on :443 for that SNI
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0015
- docs/architecture.md §7.3