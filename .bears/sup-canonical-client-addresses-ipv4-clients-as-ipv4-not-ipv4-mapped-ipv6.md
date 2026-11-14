---
id: sup
title: "Canonical client addresses: IPv4 clients as IPv4, not IPv4-mapped IPv6"
status: done
priority: P2
created: "2026-11-14T21:53:40.965642107Z"
updated: "2026-11-14T22:07:04.685627307Z"
tags:
  - proxy
---

## Summary
User feedback of 2026-11-14: the proxy listens dual-stack, so an IPv4 client's peer address is IPv4-mapped IPv6 and `X-Forwarded-For` carries `::ffff:90.129.95.63`. Upstreams then log, rate-limit (an IPv6 /64 mask puts every IPv4 client in `::ffff:0:0/64`) and match trusted-proxy CIDRs on the mapped form. Inside hoster one client should have one representation everywhere.

## Acceptance Criteria
- [x] `Forwarding::client_ip` (`src/proxy/router.rs`) returns `IpAddr::to_canonical()`; this feeds `X-Forwarded-For`, the access record (request rows in `logs.db`) and the OTLP receiver's peer check
- [x] The admission key canonicalises: `Admission`'s `Key::from` (the one entry point for both the filter and `claim`) already unmapped; it now uses `to_canonical()`
- [x] The local address in `src/proxy/plain.rs` is canonicalised too; the receiver's own `to_canonical` calls stay
- [x] Request rows stored before the change read back canonical (`src/store/logs/query/requests.rs`), so `src/web/logs/access.rs` and downloads need no unmapping of their own
- [x] Tests: an IPv4 client of a `[::]` listener appears as `127.0.0.1` in XFF, the request row and the admission key (fails without the fix with `::ffff:127.0.0.1`); a mapped row reads back as IPv4. Existing XFF tests unchanged (they already expected plain IPv4 on an IPv4 listener)
- [x] Documented in architecture §6 (forwarded headers) and the stack-format wiki's forwarding-headers table; ADR-0010 not edited

## References
- ADR-0010, ADR-0065 (per-address key), `docs/architecture.md` §6 (grep `X-Forwarded-For`)
- `src/proxy/router.rs` (`Forwarding`), `src/proxy/admission.rs` (`Key`), `src/proxy/plain.rs`, `src/proxy/receiver.rs`, `src/store/logs/query/requests.rs`