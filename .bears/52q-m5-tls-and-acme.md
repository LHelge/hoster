---
id: "52q"
title: "M5: TLS and ACME"
type: epic
status: done
priority: P1
created: "2026-11-07T14:45:17.140929949Z"
updated: "2026-11-08T02:35:56.809567846Z"
---

## Scope

rustls TLS on :443 with a custom ResolvesServerCert over ArcSwap'd SNI map; :80 ACME HTTP-01 + redirect; instant-acme issuance, persistence and background renewal. ADR-0014..0016.

## Acceptance Criteria

- [ ] Certificates are issued against Pebble in tests and against LE staging manually, persisted, and hot-swapped without restart
- [ ] Renewal runs in the background