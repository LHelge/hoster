---
id: nra
title: ":443 rustls listener and :80 challenge/redirect service"
status: done
priority: P1
created: "2026-11-07T14:45:17.262028217Z"
updated: "2026-11-08T00:07:46.158910Z"
tags:
  - tls
  - proxy
depends_on:
  - erg
  - wks
  - ngr
parent: "52q"
attempts: 1
---

## Summary
Attach the proxy service to a TLS listener using `TlsSettings::set_cert_resolver` and add the port-80 service (ADR-0015).

## Acceptance Criteria
- [ ] `TlsSettings` with resolver, `enable_h2()` (ADR-0033), added via `add_tls_with_settings("[::]:443", …)`
- [ ] :80 service: `/.well-known/acme-challenge/<token>` answered from `ArcSwap<ChallengeMap>`, everything else 308 to https (404 for hosts not in the RouteTable, ADR-0032)
- [ ] Host vs SNI mismatch handled per ADR-0032 (e.g. 421)
- [ ] Listen addresses configurable for tests

## Testing
- Integration tests with rustls client + self-signed certs: correct cert per SNI, h2 negotiated, unknown SNI fails handshake, redirect from :80 preserves path/query
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §7.1, §7.2
- ADR-0014, ADR-0015