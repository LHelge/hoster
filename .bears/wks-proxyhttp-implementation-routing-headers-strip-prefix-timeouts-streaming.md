---
id: wks
title: "ProxyHttp implementation: routing, headers, strip_prefix, timeouts, streaming"
status: open
priority: P1
created: "2026-11-07T14:45:17.241245548Z"
updated: "2026-11-07T14:45:17.241245548Z"
tags:
  - proxy
depends_on:
  - whw
  - vhg
  - ngr
parent: up4
---

## Summary
Implement the `ProxyHttp` service that routes against `ArcSwap<RouteTable>` with the header semantics of ADR-0010 and per-route timeouts of ADR-0011.

## Acceptance Criteria
- [ ] `request_filter`/`early_request_filter`: load RouteTable snapshot once per request into CTX; unknown host/prefix → built-in responses per ADR-0032
- [ ] `upstream_peer`: HttpPeer to `127.0.0.1:<port>` with PeerOptions connection/read/write/idle timeouts from the route (defaults from config)
- [ ] `upstream_request_filter`: Host unchanged; strip prefix if configured; X-Forwarded-Prefix (not for '/'); X-Forwarded-For appended; X-Forwarded-Proto/Host set; client values overwritten
- [ ] Bodies streamed (no buffering filters); upgrade/WebSocket passes through
- [ ] 502 page on upstream connect failure; no retries after body sent

## Edge Cases & Considerations
- HTTP/2 :authority vs Host
- Absolute-form request targets
- Very long paths

## Testing
- Integration tests with in-process test upstreams (hyper/axum) on loopback: path/header assertions, strip on/off, 1 GiB streamed upload/download with bounded memory, SSE stream, timeout behaviour
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §6.3
- ADR-0010, ADR-0011