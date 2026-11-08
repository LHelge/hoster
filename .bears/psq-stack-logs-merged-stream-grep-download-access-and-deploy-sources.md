---
id: psq
title: "Stack logs: merged stream, grep, download, access and deploy sources"
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:30.024239523Z"
updated: "2026-11-08T18:22:30.024239523Z"
tags:
  - design-followup
  - observability
  - web
---

Stub from the design pass (epic 8bf). Break down when picked up.

## What

The rest of `docs/design/StackLogs.dc.html`: an **all services** view merging every container's stream with a coloured service column; a **grep** filter (substring, optional regex); a **level** filter (heuristic parse of common formats); **download** of the shown range; and a **log source** switch between containers, **access log** (the proxy's per-request log for the stack's routes) and **deploy** (deployment transitions and output).

## Why it isn't in the design pass

The logs tab streams one container at a time (ADR-0044). Merging, server-side filtering and the access log each need new backend.

## Size and shape

Medium to large. Merging N bollard log streams ordered by timestamp is medium. Grep and download are small once streams are merged. The access log is larger: the proxy logs requests to the journal today, and showing them per stack needs either a journal reader or an in-memory ring buffer per route. That part needs a decision.

References: ADR-0044, ADR-0045, `src/web/containers.rs`, `src/logging.rs`, `src/proxy/`.