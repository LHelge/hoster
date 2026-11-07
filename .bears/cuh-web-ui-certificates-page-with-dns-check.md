---
id: cuh
title: "Web UI: certificates page with DNS check"
status: open
priority: P3
created: "2026-11-07T14:45:17.279477441Z"
updated: "2026-11-07T21:50:54.280578169Z"
tags:
  - web
  - tls
depends_on:
  - a9y
  - "3g2"
parent: "52q"
---

## Summary
Show certificate status per host and help diagnose issuance failures (ADR-0036, ADR-0051 #2).

## Acceptance Criteria
- [ ] Table: host, issuer, not_after, last error, next attempt, DNS status (ok / mismatch with observed vs expected addresses, from the ACME task's advisory check)
- [ ] Page header shows hoster's own public addresses (resolved from the admin host or `acme.public_addresses`) so the operator sees what the check compares against
- [ ] "Check DNS now" action re-runs the comparison for one host
- [ ] Manual "retry now" action (respects CA rate-limit safety: refused if the host's backoff has not elapsed, with the time left shown)

## Testing
- Handler/template tests with fake status including a mismatch row
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0036, ADR-0051