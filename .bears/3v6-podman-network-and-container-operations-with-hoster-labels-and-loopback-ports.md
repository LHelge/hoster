---
id: "3v6"
title: Podman network and container operations with hoster labels and loopback ports
status: open
priority: P1
created: "2026-11-07T14:45:17.175446334Z"
updated: "2026-11-07T14:45:17.175446334Z"
tags:
  - runtime
depends_on:
  - usu
parent: "5ch"
---

## Summary
Implement network and container lifecycle operations in `PodmanRuntime` following the naming and label conventions in architecture §8.

## Acceptance Criteria
- [ ] ensure_network(`hoster-<stack>`) idempotent, labelled `hoster.managed=true`, `hoster.stack`
- [ ] create_container from a `ContainerSpec`: name `hoster-<stack>-<svc>-g<gen>`, full label set, network + alias `<svc>`, env, command/entrypoint, port bindings **only** as `{HostIp: 127.0.0.1, HostPort: <allocated>}`, restart policy `no`, stop timeout
- [ ] start/stop(timeout)/remove(force)/inspect/list_managed(filter by labels) implemented
- [ ] No code path can set privileged, host network/PID/IPC, devices, cap_add, or bind mounts outside named volumes (ADR-0019) — enforced by the ContainerSpec type
- [ ] Integration test proves: published port answers on 127.0.0.1 and is not bound on 0.0.0.0 (inspect HostIp + `ss`-style check)

## Implementation Notes
- Reference probe in architecture §1.5 (compat API PortBindings/NetworkMode)
- Use `hoster.digest` label for the *requested* digest

## Edge Cases & Considerations
- Name conflict on create (leftover from crash) → surface a typed error the reconciler can handle
- Removing a network with attached containers → typed error

## Testing
- Gated integration tests against real podman using a tiny public image (e.g. busybox httpd) pulled once
- Clean up all test objects by label
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §1.5, §8
- ADR-0006, ADR-0007, ADR-0019