---
id: qnh
title: Trace waterfall view in the admin UI
type: epic
status: open
priority: P3
created: "2026-11-12T13:14:07.275145457Z"
updated: "2026-11-12T13:14:07.275145457Z"
tags:
  - observability
  - web
depends_on:
  - xv9
  - usf
---

Stub from the telemetry investigation of 2026-11-12. Break down when picked up; needs a design first (there is no mockup in `docs/design/`).

## What

- A **trace page**: one request as a waterfall. The proxy's span (epic `usf`) is the root; spans received over OTLP (epic `xv9`) nest under it by parent id, as bars on a shared time axis with service, name, duration and status; selecting a span shows its attributes and the log records that carry its span id.
- **Ways in**: from an access-log row and from any log record with a trace id (epic `psq`); a "slowest requests" and "failed requests" list per stack over a time range.
- A trace whose app sent no spans still shows the proxy's bar, so the page works for every request.

## Why

Seeing where one request spent its time across the proxy and several services is the one thing logs cannot show, and the reason to receive OTLP at all.

## Size and shape

Medium to large, mostly UI. The CSP allows no inline styles (ADR-0054), so the waterfall is server-drawn SVG like the traffic charts (ADR-0062). Traces with hundreds of spans need collapsing or a cap. Spans arrive out of order and late, so the page must render partial traces.

References: OQ-42, ADR-0054, ADR-0062, ADR-0021, `src/web/`, `docs/design/README.md`.