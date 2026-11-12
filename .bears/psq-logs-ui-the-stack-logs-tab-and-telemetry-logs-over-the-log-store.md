---
id: psq
title: "Logs UI: the stack logs tab and Telemetry → Logs over the log store"
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:30.024239523Z"
updated: "2026-11-12T19:35:43.014141451Z"
tags:
  - design-followup
  - observability
  - web
depends_on:
  - f8z
---

From the design pass (epic 8bf), rewritten on 2026-11-12 after the telemetry investigation and broken down the same day, after OQ-41 was decided as ADR-0067. Widened the same day to the Logs tab of `docs/design/Telemetry.dc.html`. Every task reads the log store that epic `f8z` builds, so nothing here is ready before it.

## What

One log view over the persisted log store, with two scopes (owner, 2026-11-12): the **stack logs tab** (`docs/design/StackLogs.dc.html`) is the view with its stack fixed, and **Telemetry → Logs** (`docs/design/Telemetry.dc.html`) is the same view across all stacks with a stack selector. Filters, fields and paging are built once.

Both scopes:

- an **all services** list in time order with a coloured service column (and a stack column across stacks), hoster's own records under the service `hoster` (ADR-0068);
- **grep** (substring, optional regex) and the **level** chips (all, error, warn+, info, debug), run by the store; text lines have no level (ADR-0067 §3);
- **structured fields**: a JSON record expands to its fields, and a field value becomes a filter;
- a **time range** (5m–7d) and paging back through history, including replaced and removed containers.

Stack scope only: **follow** over SSE, **download**, and the **source** switch between containers, **access log** (the proxy's request rows, epic `usf`) and **deploy**.

Telemetry scope only: the Telemetry nav item and page shell, and the **severity histogram**.

The query box is grep with filter controls beside it; a query language is OQ-43 and epic `ddf`. Trace links (a record's trace id, `?trace=` filter, "logs in trace") are added by epic `qnh` once spans exist.

## Size and shape

Medium to large. The access-log source waits for the request-row tasks of `usf` (`z7r`, `qr7`), not for its trace-context task.

## Tasks

`yn7` (shared view, stack tab) first; then `btj` (level and grep), `gfn` (follow), `qs4` (fields), `cb6` (source switch, deploy) and `ckc` (Telemetry page, Logs tab) in any order; `gz9` (download) after `btj`; `bq6` (access log) after `cb6`, `z7r` and `qr7`; `hsj` (histogram) after `ckc`.

References: ADR-0067, ADR-0068, ADR-0044, ADR-0045, ADR-0054, ADR-0021, OQ-43, `docs/design/StackLogs.dc.html`, `docs/design/Telemetry.dc.html`, `src/web/containers.rs`, `src/web/stacks/logs.rs`, `src/logging.rs`.