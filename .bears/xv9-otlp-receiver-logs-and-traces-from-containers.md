---
id: xv9
title: "OTLP receiver: logs and traces from containers"
type: epic
status: open
priority: P3
created: "2026-11-12T13:13:40.171211833Z"
updated: "2026-11-13T17:02:02.965432366Z"
tags:
  - observability
  - runtime
  - stack
depends_on:
  - f8z
---

From the telemetry investigation of 2026-11-12. Decided in ADR-0069 on 2026-11-13 (task `yjp`, after spike `h4q`); the tasks were drafted on 2026-11-12 and amended to the ADR in `yjp`'s commit.

## What

hoster accepts OpenTelemetry data pushed by containers over OTLP/HTTP protobuf (`POST /v1/logs`, `/v1/traces`) and stores it in the log store (epic `f8z`): log records beside the stdout records, spans in their own table, kept 3 days. It is a receiver with storage, not an OpenTelemetry Collector: no pipelines, no export, no metrics. Containers reach it on the proxy's `:80` as `host.containers.internal`, which the proxy serves only to peers with one of the host's own addresses. Every managed service gets a derived token and the `OTEL_*` variables that point its SDK at the receiver. The proxy starts a trace for every request and sends `traceparent` upstream, so an app's spans nest under the proxy's.

## Why

stdout JSON gives logs but no trace waterfall: that needs spans with start, end, parent and a shared trace id. The owner's Rust apps can emit them through `tracing-opentelemetry`, and `aphid-server` 0.4.1 already can. The trace views (`qnh`, `docs/design/Telemetry.dc.html`) are built on this.

## Size and shape

Large. The injected variables change every service's config hash once, so the first deployment of each stack after the upgrade replaces all its services; the release notes must say so. Two things are still to be shown, both in `5up`: that Pingora can match the receiver route before the HTTPS redirect, and that outside clients fail the peer check (to be tested from a second machine). Per-service token rotation is out of scope (ADR-0069 §4).

## Tasks

`h4q` (spike) → `yjp` (decision, ADR-0069). Then `aqy` (span table, request row columns) and `j6z` (derived token) in either order; `m7x` (receiver handlers) after both; `ata` (rate caps) after `m7x`; `5up` (the route on `:80`, injected variables) after `m7x`; `buj` (trace context at the proxy) after `aqy` and the request rows (`z7r`); `gv2` (docs) last.

References: ADR-0069, `docs/spikes/container-to-host-listener.md`, ADR-0010, ADR-0018, ADR-0029, ADR-0032, ADR-0045, ADR-0051, ADR-0065, ADR-0067, ADR-0068, `src/web/`, `src/runtime/podman.rs`, `src/stack/`, `src/proxy/router.rs`.