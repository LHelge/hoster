---
id: usf
title: Edge request log and trace context at the proxy
type: epic
status: open
priority: P3
created: "2026-11-12T13:13:40.148023110Z"
updated: "2026-11-12T13:13:40.148023110Z"
tags:
  - observability
  - proxy
depends_on:
  - f8z
---

Stub from the telemetry investigation of 2026-11-12. Break down when picked up. The request rows are specified by ADR-0067 §5; the `traceparent` part follows the owner's answers to OQ-42 (always start a trace, replace a client's) and waits for its ADR.

## What

The proxy records one row per request in the log store (epic `f8z`): time, host, prefix, stack, service, method, path, status, duration to the response header and in total, bytes, client address, upstream error. It gives every request an id, sends it upstream as a header so apps can log it, and stores it with the row. If OQ-42 is decided for OTLP, the same id is a W3C trace id sent as `traceparent`, and the row is the trace's root span.

This is the **access log** source that `psq` shows, and the root span the waterfall epic draws.

## Why

The proxy logs requests to the journal today, which the UI cannot read per stack (the open decision in the old `psq` stub). Traffic history (ADR-0062) is per route and per minute, so it cannot answer "which paths 404" or "show me this slow request". For static sites the proxy is the only telemetry source worth having.

## Size and shape

Medium. The hook points exist (`src/stats/` is fed from the same place). Care is needed on the hot path: rows go through a bounded channel to the recorder and are dropped, with a counter, when it is full; the proxy never waits on the store. Whether a client-supplied `traceparent` or request id is honoured is part of OQ-42 and relates to OQ-38.

References: ADR-0067 (§5), OQ-42 (question 6), OQ-38, ADR-0008, ADR-0010, ADR-0045, ADR-0062, `src/proxy/`, `src/stats/`, `src/logging.rs`.