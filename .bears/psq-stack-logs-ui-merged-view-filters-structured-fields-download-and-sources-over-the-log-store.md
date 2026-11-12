---
id: psq
title: "Stack logs UI: merged view, filters, structured fields, download and sources over the log store"
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:30.024239523Z"
updated: "2026-11-12T13:14:26.625202136Z"
tags:
  - design-followup
  - observability
  - web
depends_on:
  - f8z
---

From the design pass (epic 8bf), rewritten on 2026-11-12 after the telemetry investigation and broken down the same day, after OQ-41 was decided as ADR-0067. Every task reads the log store that epic `f8z` builds, so nothing here is ready before it.

## What

The rest of `docs/design/StackLogs.dc.html`, built as queries on the persisted log store instead of on live podman streams:

- an **all services** view of a stack's records in time order with a coloured service column;
- a **grep** filter (substring, optional regex) and a **level** filter, both run by the store. Levels come from parsed JSON lines; text lines have none (ADR-0067 §3);
- **structured fields**: a record that was a JSON line expands to its fields, and a field value can be used as a filter;
- a **time range** and paging back through history, including the logs of replaced and removed containers, with the deploy generation shown;
- **follow**: new records streamed over SSE as today;
- **download** of the shown range;
- a **log source** switch between containers, **access log** (the proxy's per-request rows, epic `usf`) and **deploy** (deployment transitions and output, already in SQLite).

Later sources plug into the same view: OTLP log records (epic `xv9`) appear beside stdout records, and a record or access-log row with a trace id links to the waterfall (epic `qnh`).

## What changed from the first stub

That stub merged N bollard streams per request and needed a decision on where an access log would come from (journal reader or ring buffer). Both are answered by the log store and the edge request log, so this epic is now UI and query work only. The one-container live view of ADR-0044 can stay as the fallback when the store is behind.

## Size and shape

Medium. The access-log source can be a second step inside the epic so the rest does not wait for `usf`.

## Tasks

`yn7` (base view) first; then `btj` (level and grep), `gfn` (follow), `qs4` (fields) and `cb6` (source switch, deploy) in any order; `gz9` (download) after `btj`; `bq6` (access log) after `cb6` and epic `usf`.

References: ADR-0067, ADR-0044, ADR-0045, ADR-0054, ADR-0021, `docs/design/StackLogs.dc.html`, `src/web/containers.rs`, `src/logging.rs`.