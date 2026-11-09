---
id: "4qj"
title: Event log and recent-events feed
type: epic
status: done
priority: P3
created: "2026-11-08T18:22:29.995791074Z"
updated: "2026-11-09T17:09:51.319608570Z"
tags:
  - design-followup
  - observability
  - web
---

A persisted event log (ADR-0055) and the three views of it from the design pass (epic 8bf).

## Scope

- One `events` table in SQLite: typed kind, severity, optional stack (no FK) and subject, message, meta. 30-day retention
- Producers write explicitly through an `EventLog` handle, then broadcast `UiEvent::Event` for live pages
- Event kinds: deploys; container lifecycle (restarts, crash loops, health); certificates; operator and system (apply/remove, secrets, startup, self-update)
- Views: the dashboard's Recent events card (`docs/design/Main.dc.html`), a per-stack feed, and an Events page with paging and filters

## Out of scope

- "5xx spike" events: depend on traffic history (`t97`)
- Notifications: `c5s`, built as a filter over this table with its own ADR

## Acceptance Criteria

- [ ] ADR-0055 accepted
- [ ] Every ADR-0055 §2 kind is recorded by its producer, with tests
- [ ] Dashboard card, per-stack feed and Events page render and update live
- [ ] Events older than 30 days are deleted
- [ ] architecture.md / spec.md describe the event log

## Order

`nbj` (store) → `yvr`, `nnd`, `nup`, `xxv` (producers, independent) and `pks` (dashboard card) → the per-stack feed + Events page task.

References: ADR-0055, ADR-0028, ADR-0036, ADR-0044, ADR-0045, `src/store/`, `src/orchestrator/`, `src/acme/`, `src/web/dashboard.rs`.