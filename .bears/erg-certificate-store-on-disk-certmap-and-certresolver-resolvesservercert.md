---
id: erg
title: Certificate store on disk, CertMap and CertResolver (ResolvesServerCert)
status: in_progress
priority: P1
created: "2026-11-07T14:45:17.257664513Z"
updated: "2026-11-07T23:09:09.043099074Z"
tags:
  - tls
depends_on:
  - vhg
  - xn6
parent: "52q"
attempts: 1
---

## Summary
Persist certificates per host (ADR-0016), load them into `ArcSwap<CertMap>` at startup, and implement `CertResolver` (ADR-0014).

## Acceptance Criteria
- [ ] Disk layout `certs/<host>/{fullchain.pem, privkey.pem (0600), meta.json}` with atomic tmp+rename writes
- [ ] Startup step 3: load all valid certs; skip/flag expired
- [ ] `CertResolver: rustls::server::ResolvesServerCert` — SNI lowercased lookup in CertMap; miss → None (per ADR-0032)
- [ ] CertifiedKey built with the ring provider (`rustls::crypto::ring::sign::any_supported_type`), matching pingora-rustls
- [ ] `insert(host, chain, key)` API used by ACME; publishes a new snapshot

## Edge Cases & Considerations
- Mismatched key/cert → rejected at insert
- Chain ordering

## Testing
- Unit tests with rcgen-generated certs; resolver returns right cert per SNI; reload replaces atomically
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §7.1, §7.4
- ADR-0014, ADR-0016