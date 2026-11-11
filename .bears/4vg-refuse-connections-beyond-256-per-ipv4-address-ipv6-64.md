---
id: "4vg"
title: Refuse connections beyond 256 per IPv4 address / IPv6 /64
status: done
priority: P1
created: "2026-11-11T21:55:25.799279871Z"
updated: "2026-11-11T23:48:45.644615149Z"
tags:
  - proxy
  - security
depends_on:
  - v5v
  - gyy
parent: "6sq"
attempts: 1
---

## What
ADR-0065 per-address cap, built on the design from the spike (v5v, `docs/spikes/per-address-connections.md`). At most 256 open connections per client IPv4 address or IPv6 /64 (IPv4-mapped IPv6 counts as IPv4). One beyond is refused at accept, in the same `ConnectionFilter` as the total cap (gyy, `Admission` in `src/proxy/admission.rs`), and counted with the same refusal metric. Constant, not config.

Per the spike: a slot is attached to each accepted L4 stream as its `tracer` (released in `on_disconnected` when Pingora drops the stream; clones inert), by a `PreTlsProcess` callback on `:443` and a claiming `ServerApp` wrapper on `:80`. The filter only checks the count.

## Acceptance
- [ ] Tests: the 257th connection from one address is refused while another address still connects (client bound to `127.0.0.2`); closing one frees a slot; h1 keep-alive and h2 count once per connection; a failed or stalled TLS handshake holds and frees a slot; two addresses in one /64 share the budget
- [ ] The spike's tests (`src/proxy/tests/address_slots.rs`) move to the real proxy harness, and the prototype there is removed
- [ ] Architecture §9.1 row updated
- [ ] Memory of the counter map is bounded (entries removed at zero)

## References
- ADR-0065; `docs/spikes/per-address-connections.md`; `src/proxy/admission.rs`, `src/proxy/mod.rs`, `src/proxy/tests/limits.rs`, `src/proxy/tests/address_slots.rs`
- pingora-core 0.9.1 `src/listeners/mod.rs` (`PreTlsProcess`), `src/protocols/l4/stream.rs` (`tracer`, `Drop`)