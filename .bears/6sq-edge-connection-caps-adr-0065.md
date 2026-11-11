---
id: "6sq"
title: Edge connection caps (ADR-0065)
type: epic
status: open
priority: P1
created: "2026-11-11T21:54:51.464844465Z"
updated: "2026-11-11T21:54:51.464844465Z"
tags:
  - proxy
  - security
---

ADR-0065 (resolves OQ-39): raise the descriptor limit, refuse new clients beyond a total cap (open descriptors) and beyond a per-address cap, count refusals, and report the missing h2 handshake timeout to Pingora. Today one client holding ~1000 idle connections stops ingress, the admin UI and deploys together.

References: ADR-0065, ADR-0008, ADR-0032, ADR-0031; `docs/architecture.md` §9.1; `src/proxy/mod.rs` (`DownstreamLimits`), `src/proxy/tests/limits.rs`, `deploy/systemd/hoster.service`, `tests/supervisor.rs`.