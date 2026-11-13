---
id: buj
title: "Trace context at the proxy: a W3C trace per request, traceparent sent upstream"
status: done
priority: P3
created: "2026-11-12T19:34:46.999621895Z"
updated: "2026-11-13T18:52:50.864683135Z"
tags:
  - proxy
  - observability
depends_on:
  - z7r
  - yjp
  - aqy
parent: xv9
attempts: 1
---

## Summary
The proxy starts a trace for every request and sends `traceparent` upstream, replacing whatever the client sent (ADR-0069 §8). The trace id is the request id that the request-row task already stores, so a container's own spans and log lines carry the id of the row. The proxy's own two spans are derived from the request row (ADR-0069 §9), so this task fills the row's span columns that `aqy` adds. Checked against ADR-0069 on 2026-11-13.

## Acceptance Criteria
- [ ] Every proxied request gets a `traceparent` header (`00-<trace id>-<span id>-01`) towards the upstream; a client-supplied `traceparent` and `tracestate` are removed first
- [ ] The trace id is the request row's request id; the parent span id sent upstream is the proxy's upstream (client) span
- [ ] The request row stores the server span id, the client span id and the time the upstream request started (columns from `aqy`)
- [ ] hoster's own records (ADR-0068) emitted inside a request carry its trace id and the proxy's span id
- [ ] `site/content/wiki/stack-format.md` (forwarded headers section) documents `traceparent` beside `X-Forwarded-*` and that the client's is replaced
- [ ] ADR-0010's list of headers hoster sets is unchanged except for this addition; ADR-0069 records it

## Implementation Notes
- Header handling sits with the `X-Forwarded-*` code in `src/proxy/router.rs` (`upstream_request_filter`)
- The `traceparent` flags are `01` (sampled): there is no sampling (ADR-0069 §7, §8)

## Testing
- Proxy tests: header added, client header replaced, ids match the stored row, WebSocket upgrades carry it too
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0069 (§8, §9), ADR-0010, ADR-0067 (§5), ADR-0068
- `src/proxy/router.rs`, `site/content/wiki/stack-format.md`