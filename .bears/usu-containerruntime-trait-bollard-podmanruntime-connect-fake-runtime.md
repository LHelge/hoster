---
id: usu
title: ContainerRuntime trait, bollard PodmanRuntime connect, fake runtime
status: open
priority: P1
created: "2026-11-07T14:45:17.172502566Z"
updated: "2026-11-07T14:45:17.172502566Z"
tags:
  - runtime
depends_on:
  - "26p"
parent: "5ch"
---

## Summary
Introduce the `ContainerRuntime` trait the deploy engine depends on, a bollard-backed `PodmanRuntime` that connects to the rootless socket, and an in-memory fake for tests.

## Acceptance Criteria
- [ ] `PodmanRuntime::connect()` uses the configured socket path or `Docker::connect_with_podman_defaults()`, then **`negotiate_version()`** and `ping()` (bollard 0.21.1 defaults to API 1.53; podman 6.1.4 reports 1.44)
- [ ] Connected API version and podman version logged at startup
- [ ] Trait methods defined (not all implemented yet): ensure_network, remove_network, pull_image, create_container, start, stop, remove, inspect, list_managed, logs
- [ ] `FakeRuntime` implements the trait with in-memory state and failure injection hooks
- [ ] Startup step 4 in the supervisor wired to this

## Implementation Notes
- bollard = "0.21" with default features (http, pipe) — no TLS needed for UDS
- Keep bollard types out of the trait signatures; define hoster-domain types (ContainerSpec, ContainerState)

## Edge Cases & Considerations
- Socket missing → clear error naming the path and pointing at `hoster doctor`

## Testing
- Integration test gated by `HOSTER_PODMAN_TESTS=1`: connect + ping against real rootless podman
- Unit tests on FakeRuntime
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §1.3, §8
- ADR-0002