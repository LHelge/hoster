---
id: g82
title: "Startup readiness probe logs a TLS handshake ERROR on :443"
status: open
priority: P3
created: "2026-11-08T11:59:22.459059706Z"
updated: "2026-11-08T11:59:22.459059706Z"
tags:
  - proxy
  - observability
---

## Summary
Every start logs `ERROR pingora_core::services::listening: Downstream handshake error from [::1]:…: TLSHandshakeFailure … tls handshake eof`. It is hoster's own readiness probe (`Probe::accepts` in src/proxy/mod.rs): a plain TCP connect to each listener that closes at once, which on the TLS listener looks like a failed handshake. Seen on the first real install. An ERROR on every healthy start trains admins to ignore errors.

## Acceptance Criteria
- [ ] A healthy start logs no ERROR: e.g. probe :443 with a real TLS ClientHello (SNI = admin host, accept any cert) and close cleanly, or rely on Pingora's ExecutionPhase alone for the TLS listener, or filter that one log line for loopback peers during startup
- [ ] Test asserting no ERROR-level line during a normal proxy start (logging capture helper)

## References
- docs/architecture.md §3.3, §4
- src/proxy/mod.rs (Probe)