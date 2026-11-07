# ADR-0033: Protocol support in v1: h1+h2 downstream, WebSockets, h1 upstream

- Status: Accepted (resolves OQ-22)
- Date: 2026-11-07

## Decision

- **Downstream:** HTTP/1.1 and HTTP/2. `enable_h2()` is set on the TLS listener, so ALPN offers
  h2 and http/1.1.
- **WebSockets:** proxied through HTTP/1.1 `Upgrade`. Long-lived connections are cut when the
  drain timeout expires during a deploy (ADR-0017).
- **Upstream:** HTTP/1.1 only. h2c, which gRPC needs, is a later per-route option
  (`protocol = "h2c"`).
- **HTTP/3:** out of scope.

## Alternatives considered

- **gRPC (h2c upstream) in v1**: there is no current need, and it adds scope to the proxy and its
  tests.

## Consequences

- gRPC backends are not supported until the per-route option exists.
- Documentation must state that WebSocket and SSE clients have to reconnect after a deploy.
