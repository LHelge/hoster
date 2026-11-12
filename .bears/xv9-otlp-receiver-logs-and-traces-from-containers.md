---
id: xv9
title: "OTLP receiver: logs and traces from containers"
type: epic
status: open
priority: P3
created: "2026-11-12T13:13:40.171211833Z"
updated: "2026-11-12T19:38:53.332284574Z"
tags:
  - observability
  - runtime
  - stack
depends_on:
  - f8z
---

From the telemetry investigation of 2026-11-12. The tasks after the spike and the decision were drafted on 2026-11-12 from the owner's answers to OQ-42. They wait for `yjp`, whose ADR they must be checked against (the listener task `5up` most of all). `yjp` amends them rather than breaking the epic down from scratch.

## What

hoster accepts OpenTelemetry data pushed by containers over OTLP/HTTP protobuf (`POST /v1/logs`, `/v1/traces`) and stores it in the log store (epic `f8z`): log records beside the stdout records, spans in their own table, kept 3 days. It is a receiver with storage, not an OpenTelemetry Collector: no pipelines, no export, no metrics. Every managed service gets a token and the `OTEL_*` variables that point its SDK at the receiver. The proxy starts a trace for every request and sends `traceparent` upstream, so an app's spans nest under the proxy's.

## Why

stdout JSON gives logs but no trace waterfall: that needs spans with start, end, parent and a shared trace id. The owner's Rust apps can emit them through `tracing-opentelemetry`, and `aphid-server` 0.4.1 already can. The trace views (`qnh`, `docs/design/Telemetry.dc.html`) are built on this.

## Size and shape

Large, and the most uncertain of the telemetry epics: it touches ADR-0018, 0044, 0045 and 0046 and depends on a networking spike.

## Tasks

`h4q` (spike) → `yjp` (decision, ADR). Then `aqy` (span table) and `j6z` (tokens) in either order; `m7x` (receiver handlers) after both; `ata` (rate caps) after `m7x`; `5up` (listener, injected variables) after `m7x` and `h4q`; `buj` (trace context at the proxy, moved here from `usf`) after `yjp` and the request rows (`z7r`); `gv2` (docs) last.

References: OQ-42, ADR-0018, ADR-0044, ADR-0045, ADR-0046, ADR-0006, ADR-0007, ADR-0029, ADR-0051, ADR-0067, ADR-0068, `src/web/`, `src/runtime/podman.rs`, `src/stack/`, `src/proxy/router.rs`.