---
id: "6gn"
title: "M8: Observability"
type: epic
status: done
priority: P2
created: "2026-11-07T14:45:17.148039506Z"
updated: "2026-11-08T03:14:19.452484181Z"
---

## Scope

Structured and access logs, in-process statistics shown in the UI (no metrics endpoint, no alerting; ADR-0045), container log streaming (ADR-0044), live dashboard, continuous health monitoring.

## Acceptance Criteria

- [ ] Operator can see stack health, deploy history, cert expiry and container logs in the UI
- [ ] Dashboard highlights failing health, crash loops, expiring certs and failed deploys