# ADR-0011: Stream bodies; timeouts configurable per route

- Status: Accepted
- Date: 2026-11-07

## Context

The proxy will carry large S3 uploads and downloads, server-sent events and long polls. Buffering
whole bodies would blow up memory and break streaming semantics. A single global timeout would be
either too short for uploads or too long for APIs.

## Decision

- Request and response bodies are streamed in both directions. hoster installs no body filters
  that buffer.
- Each route can set its own `connect`, `read`, `write` and `idle` timeouts. These map to
  Pingora's `PeerOptions` on the `HttpPeer` (verified fields in 0.9.1: `connection_timeout`,
  `total_connection_timeout`, `read_timeout`, `write_timeout`, `idle_timeout`). Unset values fall
  back to hoster-wide defaults.

## Alternatives considered

- **Buffering with size limits**: simpler retry logic, but breaks SSE and large uploads. Rejected.
- **Global timeouts only**: forces a bad compromise. Rejected.

## Consequences

- Retrying requests that already have a body sent is not possible in general. hoster does not
  retry upstream requests once the body has started.
- Draining can be held open by long streams. The drain timeout bounds it (ADR-0017).
- A request body size limit, if one is wanted, is a separate per-route setting and does not mean
  buffering (ADR-0035 #9).
