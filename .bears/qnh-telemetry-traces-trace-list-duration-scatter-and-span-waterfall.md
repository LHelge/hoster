---
id: qnh
title: "Telemetry → Traces: trace list, duration scatter and span waterfall"
type: epic
status: done
priority: P3
created: "2026-11-12T13:14:07.275145457Z"
updated: "2026-11-13T23:30:16.623347310Z"
tags:
  - observability
  - web
depends_on:
  - xv9
  - usf
---

From the telemetry investigation of 2026-11-12, broken down the same day from `docs/design/Telemetry.dc.html` (traces tab), which the owner added that day. It waits for the OTLP receiver (`xv9`): the owner chose on 2026-11-12 not to ship proxy-only traces before it.

## What

The Traces tab of the Telemetry page (the page and its Logs tab are built in epic `psq`, task `ckc`):

- a **trace list** over the range with sort (newest, slowest, errors first), services as coloured chips, span count, duration bar and status; "app not instrumented" for traces with only the proxy's spans;
- a **duration scatter** of root spans on a log scale, errors in red, listed traces clickable;
- the **waterfall** of the selected trace: header figures, service legend, minimap, rows in DFS order with bars on a shared axis, error rings, collapse and "collapse to services", the critical-path highlight;
- **inline span details**: timing, self time, ids, status, attributes, resource, events with exception stacks, and the span's log records;
- **links both ways** between traces and log records (trace column, `?trace=` filter, "logs in trace"), and from access-log rows.

The proxy's spans come from the request rows or from span rows, as OQ-42 question 8 decides; the UI does not care which.

## Size and shape

Medium to large, mostly UI. The CSP allows no inline styles and no new script (ADR-0054, ADR-0060), so charts and bars are server-drawn SVG, collapse state is in the URL, and the critical path is CSS (`:has()`) with a server fallback. Waterfall math (DFS order, depth, critical path, self time) lives on the trace type, tested against the mockup's sample traces.

## Tasks

`txt` (store queries) first; `zyf` (Traces tab, list) after it and `ckc`; then `qxa` (scatter) and `aum` (waterfall) in either order; `zg4` (controls) and `jpq` (span details) after `aum`; `9x4` (links both ways) last.

References: OQ-42, ADR-0054, ADR-0060, ADR-0062, ADR-0067, ADR-0021, `docs/design/Telemetry.dc.html`, `docs/design/README.md`, `src/web/`, `src/web/charts.rs`.