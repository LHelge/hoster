---
id: "4vg"
title: Refuse connections beyond 256 per IPv4 address / IPv6 /64
status: open
priority: P1
created: "2026-11-11T21:55:25.799279871Z"
updated: "2026-11-11T21:55:35.854824746Z"
tags:
  - proxy
  - security
depends_on:
  - v5v
  - gyy
parent: "6sq"
---

## What
ADR-0065 per-address cap, built on the design from the spike (v5v). At most 256 open connections per client IPv4 address or IPv6 /64 (IPv4-mapped IPv6 counts as IPv4). One beyond is refused at accept, in the same `ConnectionFilter` as the total cap (gyy), and counted with the same refusal metric. Constant, not config.

## Acceptance
- [ ] Tests: the 257th connection from one address is refused while another address still connects; closing one frees a slot; h1 keep-alive and h2 count once per connection; two addresses in one /64 share the budget
- [ ] Architecture §9.1 row updated
- [ ] Memory of the counter map is bounded (entries removed at zero)

## References
- ADR-0065; the spike note from v5v in `docs/spikes/`; `src/proxy/mod.rs`, `src/proxy/tests/limits.rs`