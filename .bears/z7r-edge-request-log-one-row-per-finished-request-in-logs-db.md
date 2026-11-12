---
id: z7r
title: "Edge request log: one row per finished request in logs.db"
status: open
priority: P2
created: "2026-11-12T19:34:37.120468119Z"
updated: "2026-11-12T19:34:37.120468119Z"
tags:
  - proxy
  - observability
depends_on:
  - r8w
parent: usf
---

## Summary
ADR-0067 §5: when the proxy finishes a request it builds one `RequestRow` and hands it to the log store's recorder through its bounded channel. The proxy never waits for the store; a full channel drops the row and counts it. The per-request journal line moves to `debug`.

## Acceptance Criteria
- [ ] In the routed proxy's `logging` hook (`src/proxy/router.rs`), a row with time, host, prefix, stack, service, method, path, status, ms to the response header, ms in total, response bytes, client address, whether the upstream failed, and a request id
- [ ] The request id is 16 random bytes in lowercase hex (the shape of a W3C trace id), made when the request arrives and kept in the request context, so the trace-context task can send it upstream as the trace id without changing the column
- [ ] Requests the proxy answers itself (404 for unknown hosts, 421, ACME challenges, redirects on :80 in `src/proxy/plain.rs`) are not stored: they have no stack, and what a client can cause without reaching an app is not stored any more than it is logged (`src/logging.rs`, "What anyone on the internet can make hoster log")
- [ ] The row is offered with `try_request`; nothing on the request path awaits the recorder. Drops are counted for the System page card
- [ ] The access line under `ACCESS_TARGET` is emitted at `debug`, no longer at `info`; the module doc of `src/logging.rs` and the logging guide say so
- [ ] Time to the response header is measured from the request's arrival to the moment the upstream response header was received; total until the last body byte was written or the request ended

## Implementation Notes
- `src/stats/` is fed from the same hook (traffic history, ADR-0062); compute the timings once and share them
- The client address is the one the proxy saw (no `X-Forwarded-For` trust); IPv6 kept in full
- Path without the query string (it can hold tokens); say so in the guide

## Edge Cases
- A request whose upstream connection failed: status as sent to the client (502/504), upstream failed set
- WebSocket upgrades: one row when the connection closes, total is the connection's lifetime; mark them via the status 101
- Client disconnects mid-body: the row is still written with the bytes sent

## Testing
- Proxy tests (`src/proxy/tests.rs`) with a test recorder: row fields for a normal request, an upstream failure, a 101, a self-answered 404 not stored, a full channel not blocking the proxy
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§5), ADR-0062, ADR-0008, ADR-0032, ADR-0010
- `src/proxy/router.rs`, `src/proxy/plain.rs`, `src/stats/`, `src/logging.rs`