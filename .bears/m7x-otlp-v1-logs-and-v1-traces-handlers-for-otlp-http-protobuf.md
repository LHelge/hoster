---
id: m7x
title: "OTLP: /v1/logs and /v1/traces handlers for OTLP/HTTP protobuf"
status: open
priority: P2
created: "2026-11-12T19:36:20.222913703Z"
updated: "2026-11-12T19:36:20.222913703Z"
tags:
  - observability
  - web
depends_on:
  - aqy
  - j6z
parent: xv9
---

## Summary
The receiver itself: Axum handlers for `POST /v1/logs` and `POST /v1/traces` that decode OTLP/HTTP protobuf (OQ-42 answer 4: no gRPC, no JSON encoding), authenticate by token, map the payload to log records and spans, and hand them to the recorder. A receiver with storage, not a Collector: no pipelines, no export, no metrics (answer 5). This task builds the router; which listener serves it is the next task.

## Acceptance Criteria
- [ ] Requests with `Content-Type: application/x-protobuf` decode with the `opentelemetry-proto` types (prost); `Content-Encoding: gzip` is accepted (SDKs default to it); other content types get 415
- [ ] `Authorization: Bearer <token>` resolves to stack and service; missing or unknown is 401 with no body detail
- [ ] Stack and service come from the token; `service.name` and other resource attributes from the payload are kept as resource attributes for display, but never decide where data is stored
- [ ] A body size limit (a constant, before and after decompression) gives 413; a malformed body gives 400
- [ ] Success answers per the OTLP spec with an `Export*ServiceResponse`; records dropped by a full channel or a rate cap are reported as `partial_success` with the rejected count, not as an error that makes SDKs retry forever
- [ ] `/v1/metrics` answers 404 (or the spec's "unimplemented") so SDKs with metrics on fail cleanly
- [ ] The handlers never await the store: they decode, map and `try_*` the recorder

## Implementation Notes
- `cargo add opentelemetry-proto --features gen-tonic-messages` (or whichever feature gives only the prost message types without tonic; check the crate's features first) and `cargo add prost` if needed; the Rust OTel crates are 0.x and change between releases, so read the current docs
- Map OTLP `SeverityNumber` 1–24 onto the five levels; `body` (AnyValue) to message, strings as they are, structured bodies as JSON
- Keep the mapping on `From`/`TryFrom` impls from the proto types to the store's record and span types

## Testing
- Handler tests with payloads built from the proto types: logs, traces, gzip, auth failures, size limits, partial success
- One round-trip test with a real exporter (`opentelemetry-otlp` HTTP exporter in a test) if it does not slow the suite much
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- OQ-42 (questions 1, 4, 5), the ADR from `yjp`, ADR-0045
- `src/web/mod.rs` (router patterns), `src/web/webhook.rs` (an unauthenticated-by-session endpoint)