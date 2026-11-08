---
id: mdd
title: "M6: Zero-downtime deploys"
type: epic
status: done
priority: P1
created: "2026-11-07T14:45:17.143338114Z"
updated: "2026-11-08T02:58:20.532967129Z"
---

## Scope

Start-first deploy state machine: start new generation, health-check, atomic swap, drain via in-flight counters, cleanup; abort and crash recovery; stop-first variant; coalescing; rollback. ADR-0017.

## Acceptance Criteria

- [ ] Under continuous load, a deploy produces zero failed requests
- [ ] A failing new version never takes traffic and leaves the old one serving