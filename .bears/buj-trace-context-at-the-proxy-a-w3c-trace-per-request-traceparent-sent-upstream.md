---
id: buj
title: "Trace context at the proxy: a W3C trace per request, traceparent sent upstream"
status: open
priority: P3
created: "2026-11-12T19:34:46.999621895Z"
updated: "2026-11-12T19:38:38.010076250Z"
tags:
  - proxy
  - observability
depends_on:
  - z7r
  - yjp
parent: xv9
---

## Summary
The proxy starts a trace for every request and sends `traceparent` upstream, replacing whatever the client sent (owner's answer to OQ-42 question 6). The trace id is the request id that the request-row task already stores, so a container's own spans and log lines carry the id of the row. Waits for the OQ-42 ADR (task `yjp`), which also decides how the proxy's own spans are stored (question 8); this task implements whichever it says.

## Acceptance Criteria
- [ ] Every proxied request gets a `traceparent` header (`00-<trace id>-<span id>-01`) towards the upstream; a client-supplied `traceparent` and `tracestate` are removed first
- [ ] The trace id is the request row's request id; the parent span id sent upstream is the proxy's upstream (client) span
- [ ] The request row stores what the ADR requires for the proxy's spans (for option A of question 8: the server span id, the client span id and the time the upstream request started)
- [ ] hoster's own records (ADR-0068) emitted inside a request carry its trace id and the proxy's span id
- [ ] `site/content/wiki/stack-format.md` (forwarded headers section) documents `traceparent` beside `X-Forwarded-*` and that the client's is replaced
- [ ] ADR-0010's list of headers hoster sets is unchanged except for this addition; the ADR from `yjp` records it

## Implementation Notes
- Header handling sits with the `X-Forwarded-*` code in `src/proxy/router.rs` (`upstream_request_filter`)
- Recheck this body against the ADR before starting; amend it in the same commit if the ADR differs

## Testing
- Proxy tests: header added, client header replaced, ids match the stored row, WebSocket upgrades carry it too
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- OQ-42 (questions 6 and 8), the ADR written by `yjp`, ADR-0010, ADR-0067 (§5), ADR-0068
- `src/proxy/router.rs`, `site/content/wiki/stack-format.md`