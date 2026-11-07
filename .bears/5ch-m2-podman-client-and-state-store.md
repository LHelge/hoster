---
id: "5ch"
title: "M2: Podman client and state store"
type: epic
status: open
priority: P1
created: "2026-11-07T14:45:17.134625134Z"
updated: "2026-11-07T14:45:17.134625134Z"
---

## Scope

ContainerRuntime trait over bollard (rootless podman socket, negotiated API version), image pulls by digest, SQLite state store via sqlx and port allocator. ADR-0002, ADR-0007, ADR-0022.

## Acceptance Criteria

- [ ] Integration tests create networks/containers with 127.0.0.1-only port bindings against real rootless podman
- [ ] State store persists stacks, deployments and port allocations across restarts