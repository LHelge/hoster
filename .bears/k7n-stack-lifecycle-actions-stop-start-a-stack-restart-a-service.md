---
id: k7n
title: "Stack lifecycle actions: stop/start a stack, restart a service"
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:30.015375275Z"
updated: "2026-11-08T18:22:30.015375275Z"
tags:
  - design-followup
  - orchestrator
  - web
---

Stub from the design pass (epic 8bf). Break down when picked up.

## What

The design's header buttons **Stop** and **Pull & restart** on the stack page, a per-service **restart** in the services table, and a "stopped" stack status (the grey `portfolio` stack in `docs/design/Stacks.dc.html`). See `docs/design/StackDetail.dc.html`.

## Why it isn't in the design pass

hoster has no stopped state. The reconciler restarts anything that isn't running (ADR-0028), and remove is the only way down. A restart has to go through the deploy lock (ADR-0040) and keep zero downtime where possible (ADR-0017).

## Size and shape

Medium. A persisted per-stack desired state (running/stopped) that the reconciler respects, with routes withdrawn (or a 503) while stopped and certificates kept. A service restart as a "same-digest redeploy" (start-first per strategy) or a plain container restart. "Pull & restart" maps onto re-resolve + redeploy.

## Starts with a decision

Stopped semantics (routes, certificates, poller/webhook behaviour while stopped) and restart-as-redeploy vs in-place restart. Needs an ADR.

References: ADR-0017, ADR-0028, ADR-0039, ADR-0040, ADR-0048, `src/orchestrator/`, `src/supervisor.rs`.