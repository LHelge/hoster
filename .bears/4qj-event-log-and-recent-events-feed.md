---
id: "4qj"
title: Event log and recent-events feed
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:29.995791074Z"
updated: "2026-11-08T18:22:29.995791074Z"
tags:
  - design-followup
  - observability
  - web
---

Stub from the design pass (epic 8bf). Break down when picked up.

## What

The dashboard's **Recent events** list (`docs/design/Main.dc.html`): deploy started/succeeded/failed with sha and trigger, a container restarted (exit code, OOM), a certificate issued/renewed/failed with the reason, and an error spike. Each entry has a time, a tone dot, text and a mono meta line, and links to its subject.

## Why it isn't in the design pass

Deployment transitions are in SQLite, but restarts, certificate outcomes and similar only go to the log. There's no queryable event history.

## Size and shape

Medium. An `events` table (kind, subject, tone, message, meta, at) with retention, written from the orchestrator, reconciler and ACME task; a dashboard card with live SSE append; maybe a per-stack filter on the stack overview. "5xx spike" detection depends on the traffic-history epic.

## Starts with a decision

Which events, retention, and whether this is also the basis for notifications (ADR-0045's named gap; see the notifications stub).

References: ADR-0028, ADR-0036, ADR-0044, ADR-0045, `src/store/`, `src/orchestrator/`, `src/acme/`.