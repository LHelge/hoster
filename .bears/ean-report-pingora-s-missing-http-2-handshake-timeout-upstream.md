---
id: ean
title: Report Pingora's missing HTTP/2 handshake timeout upstream
status: open
priority: P2
created: "2026-11-11T21:55:25.810404307Z"
updated: "2026-11-11T21:55:25.810404307Z"
tags:
  - proxy
  - upstream
parent: "6sq"
---

## What
ADR-0065 (F): Pingora 0.9.1 has no timeout around the HTTP/2 handshake. A client that negotiates `h2` via ALPN and never sends the connection preface keeps the connection open forever, since `h2_idle_timeout` starts after the handshake (architecture §9.1 row "Negotiates `h2` and never sends the connection preface"). Under ADR-0065's caps it costs one slot.

## Do
- Check cloudflare/pingora issues and the latest release first. If it is already fixed, file a task to upgrade instead.
- Otherwise open an issue with a minimal reproduction (the existing test in `src/proxy/tests/limits.rs`) and, ideally, where a timeout could go. **This posts publicly under the owner's account: get the owner's OK on the text before filing.**
- Link the issue from architecture §9.1.

## Acceptance
- [ ] Issue filed (or an existing one linked) and referenced in §9.1

## References
- ADR-0065; `docs/architecture.md` §9.1; `src/proxy/tests/limits.rs`