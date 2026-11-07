---
id: hez
title: ACME account management with instant-acme
status: done
priority: P1
created: "2026-11-07T14:45:17.266340654Z"
updated: "2026-11-07T22:30:24.588495124Z"
tags:
  - tls
  - acme
depends_on:
  - xn6
  - "26p"
parent: "52q"
attempts: 1
---

## Summary
Create or restore the ACME account and persist its credentials (ADR-0016).

## Acceptance Criteria
- [ ] instant-acme 0.8 with default-features=false and features ring, hyper-rustls, rcgen, time (ADR-0037); CI asserts aws-lc-rs is not in the dependency tree
- [ ] Load `acme/account.json` (AccountCredentials) via `from_credentials`, else `create(NewAccount{contact, tos agreed})` and persist 0600
- [ ] Directory URL from config (LE staging/prod/Pebble); account bound to directory (changing directory creates a new account file)

## Testing
- Integration test against Pebble container (gated): create, persist, restore
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §1.2, §7.3