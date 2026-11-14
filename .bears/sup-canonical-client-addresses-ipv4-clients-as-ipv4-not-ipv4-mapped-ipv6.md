---
id: sup
title: "Canonical client addresses: IPv4 clients as IPv4, not IPv4-mapped IPv6"
status: open
priority: P2
created: "2026-11-14T21:53:40.965642107Z"
updated: "2026-11-14T21:53:40.965642107Z"
tags:
  - proxy
---

## Summary
User feedback of 2026-11-14: the proxy listens dual-stack, so an IPv4 client's peer address is IPv4-mapped IPv6 and `X-Forwarded-For` carries `::ffff:90.129.95.63`. Upstreams then log, rate-limit (an IPv6 /64 mask puts every IPv4 client in `::ffff:0:0/64`) and match trusted-proxy CIDRs on the mapped form. Inside hoster one client should have one representation everywhere.

## Acceptance Criteria
- [ ] `Forwarding::client_ip` (`src/proxy/router.rs`) returns `IpAddr::to_canonical()`; this feeds `X-Forwarded-For`, the access record (request rows in `logs.db`) and the OTLP receiver's peer check
- [ ] `Admission::claim` (`src/proxy/admission.rs`) canonicalises before keying; `Key::from`'s own unmapping can then go
- [ ] The local address in `src/proxy/plain.rs` is canonicalised too; the receiver's own `to_canonical` calls stay correct
- [ ] `src/web/logs/access.rs` keeps unmapping for rows stored before the change
- [ ] Tests: an IPv4 client over the dual-stack listener appears as plain IPv4 in XFF and in the admission key; a real IPv6 client is unchanged. Existing XFF tests (`src/proxy/tests.rs`, `src/proxy/tests/smuggling.rs`, router unit tests) updated
- [ ] Documented in architecture §6 (forwarded headers) and the stack-format wiki where it lists the forwarding headers; ADR-0010 is not edited (immutable) unless a Correction is warranted

## References
- ADR-0010, ADR-0065 (per-address key), `docs/architecture.md` §6 (grep `X-Forwarded-For`)
- `src/proxy/router.rs` (`Forwarding`), `src/proxy/admission.rs` (`claim`, `Key`), `src/proxy/plain.rs`, `src/proxy/receiver.rs`, `src/web/logs/access.rs`