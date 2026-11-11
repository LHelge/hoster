---
id: v5v
title: "Spike: count open connections per client address in Pingora 0.9.1 (ServerApp wrapper)"
status: open
priority: P1
created: "2026-11-11T21:55:08.893921448Z"
updated: "2026-11-11T21:55:08.893921448Z"
tags:
  - proxy
  - spike
parent: "6sq"
---

## Question
ADR-0065's per-address cap needs an accurate count of open connections per client address. `ConnectionFilter::should_accept` has no close callback. Does a wrapper around Pingora's `ServerApp` (around hoster's `HttpProxy` services) that owns the keep-alive loop (`process_new` returns `Some(stream)` for reuse) count each connection exactly once and release it on every end, both for HTTP/1.1 keep-alive and HTTP/2, on TLS and plain, including shutdown and errors? Or is there a better hook (a `Stream` wrapper whose `Drop` releases, a digest-carried guard, …)?

## Deliver
- A throwaway proof against pingora 0.9.1 with tests for: h1 keep-alive across several requests (counted once), h2 with many streams (once), client abort mid-request, TLS handshake failure, graceful shutdown. The count returns to zero in each case.
- A note in `docs/spikes/` with the design to build, and how `should_accept` and the counter cooperate (the check happens at accept, the count after the stream reaches the app).
- If no reliable hook exists: say so, and raise it in `docs/open-questions.md` with the fallbacks (e.g. count in `should_accept` and release on a timer, or patch pingora).

## References
- ADR-0065; pingora-core 0.9.1 `src/apps/mod.rs` (`ServerApp`), `src/services/listening.rs`, `src/listeners/connection_filter.rs`; `src/proxy/mod.rs`