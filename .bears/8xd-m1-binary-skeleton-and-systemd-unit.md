---
id: "8xd"
title: "M1: Binary skeleton and systemd unit"
type: epic
status: open
priority: P1
created: "2026-11-07T14:45:17.131876866Z"
updated: "2026-11-07T14:45:17.131876866Z"
---

## Scope

Single `hoster` binary with config, tracing, lifecycle/signal handling, a preflight `doctor` command and an installable systemd unit. ADR-0001.

## Acceptance Criteria

- [ ] `hoster` starts under systemd as a user service per ADR-0023, logs to the user journal, shuts down cleanly on SIGTERM
- [ ] `hoster doctor` verifies podman socket, port binding and state dir