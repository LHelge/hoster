---
id: xv9
title: "OTLP receiver: logs and traces from containers"
type: epic
status: open
priority: P3
created: "2026-11-12T13:13:40.171211833Z"
updated: "2026-11-12T13:13:40.171211833Z"
tags:
  - observability
  - runtime
  - stack
depends_on:
  - f8z
---

Stub from the telemetry investigation of 2026-11-12. Break down once the spike and OQ-42 (child tasks) are done.

## What

hoster accepts OpenTelemetry data pushed by containers over OTLP/HTTP protobuf (`POST /v1/logs`, `/v1/traces`) and stores it in the log store (epic `f8z`): log records beside the stdout records, spans in their own table. It is a receiver with storage, not an OpenTelemetry Collector: no pipelines and no export.

Expected parts, to be confirmed by the decision:

- The receiver: Axum handlers decoding `opentelemetry-proto` types, size limits, per-service rate caps, answering fast and never blocking on the store.
- The path from a container to the receiver (the spike decides).
- Identity: a per-service token; hoster injects `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` (as it injects `PORT`), probably opt-in per service in the stack file because injected env changes the config hash (ADR-0029).
- Span storage: trace id, span id, parent, name, kind, start, duration, status, attributes; lookup by trace id; shorter retention than logs.
- Docs: a wiki page with the `tracing` setup for a Rust app, and `aphid-server` as the worked example.

Metrics are not in this epic (they overlap ADR-0062 and ADR-0066; revisit after traces are in use).

## Why

stdout JSON gives logs but no trace waterfall: that needs spans with start, end, parent and a shared trace id. The owner's Rust apps can emit them through `tracing-opentelemetry`, and `aphid-server` 0.4.1 already can.

## Size and shape

Large, and the most uncertain of the telemetry epics: it touches ADR-0018, 0044, 0045 and 0046 and depends on a networking spike. Worth starting only after the owner has tried OTel output locally and wants traces.

References: OQ-42, ADR-0018, ADR-0044, ADR-0045, ADR-0046, ADR-0006, ADR-0007, ADR-0029, ADR-0051, `src/web/`, `src/runtime/podman.rs`, `src/stack/`.