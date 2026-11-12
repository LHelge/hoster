---
id: usf
title: "Edge request log: one row per proxied request"
type: epic
status: open
priority: P3
created: "2026-11-12T13:13:40.148023110Z"
updated: "2026-11-12T19:38:38.020507073Z"
tags:
  - observability
  - proxy
depends_on:
  - f8z
---

From the telemetry investigation of 2026-11-12, broken down the same day. The request rows are specified by ADR-0067 §5. The trace-context part that this stub used to hold (task `buj`: start a trace per request, replace a client's `traceparent`) moved to epic `xv9`, because it waits for the OQ-42 ADR (task `yjp`) and would otherwise hold this epic's PR; the owner chose on 2026-11-12 not to split the edge part into an ADR of its own.

## What

The proxy records one row per finished request in the log store (epic `f8z`): time, host, prefix, stack, service, method, path, status, duration to the response header and in total, bytes, client address, upstream error and a request id. The request id has the shape of a W3C trace id from the start, so `buj` can send it upstream as the trace id without changing the column. The per-request journal line moves to `debug`.

This is the **access log** source of the stack logs tab (`bq6` in epic `psq`), and later the basis of the proxy's spans in the trace views (epic `qnh`, as OQ-42 question 8 decides).

## Why

The proxy logs requests to the journal today, which the UI cannot read per stack. Traffic history (ADR-0062) is per route and per minute, so it cannot answer "which paths 404" or "show me this slow request". For static sites the proxy is the only telemetry source worth having.

## Size and shape

Small to medium. Care is needed on the hot path: rows go through the recorder's bounded channel and are dropped, counted, when it is full; the proxy never waits on the store.

## Tasks

`z7r` (rows) and `qr7` (query API), in either order, once the log store's recorder (`r8w`) and query API (`gh4`) exist.

References: ADR-0067 (§5), ADR-0068, OQ-42 (questions 6 and 8), OQ-38, ADR-0008, ADR-0010, ADR-0045, ADR-0062, `src/proxy/`, `src/stats/`, `src/logging.rs`.