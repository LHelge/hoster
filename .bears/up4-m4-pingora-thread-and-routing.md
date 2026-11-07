---
id: up4
title: "M4: Pingora thread and routing"
type: epic
status: open
priority: P1
created: "2026-11-07T14:45:17.138767920Z"
updated: "2026-11-07T14:45:17.138767920Z"
---

## Scope

Embed Pingora on a dedicated thread, route by host + longest segment prefix over an ArcSwap'd RouteTable, header semantics, per-route timeouts, streaming. ADR-0008..0012.

## Acceptance Criteria

- [ ] HTTP requests for the example stack reach the right containers with correct paths and headers
- [ ] RouteTable updates on deploy/remove are atomic
- [ ] Admin UI reachable via reserved admin host