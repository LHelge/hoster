# ADR-0002: Manage containers through the rootless podman socket with bollard

- Status: Accepted
- Date: 2026-11-07

## Context

We need a container runtime that runs without root, is daemonless at rest, and that we can drive
programmatically from Rust with typed requests and streaming (pull progress, logs).

## Decision

Use **rootless podman**, accessed via its Docker-compatible REST API on the per-user socket
(`$XDG_RUNTIME_DIR/podman/podman.sock`), through the **bollard** crate (verified 0.21.1).
All runtime calls go through a `ContainerRuntime` trait so the deploy engine can be tested
against a fake.

## Alternatives considered

- **Docker Engine (rootful)**: daemon runs as root; a socket compromise is root on the host.
  Rejected.
- **Shelling out to the `podman` CLI**: easy to start, but stringly-typed, fragile output parsing,
  awkward streaming and error handling. Rejected.
- **podman's native libpod REST API** (`/v5.x/libpod/...`) via a hand-written client: exposes
  podman-only features (pods, quadlet-ish options) but there is no maintained Rust client; we'd own
  the HTTP/JSON layer. Kept as an escape hatch for individual calls if the compat API lacks
  something (bollard can't express it, we add a small raw call).
- **Rootful podman**: same root concern as Docker.

## Consequences

- Verified: podman 6.1.4 advertises compat `ApiVersion` 1.44 while bollard 0.21.1 defaults to 1.53,
  so the client **must** call `negotiate_version()` (or pin 1.44) after connecting.
- Container IPs live in the rootless network namespace and are not reachable from the host;
  the proxy reaches containers only via `127.0.0.1` published ports (ADR-0007).
- The socket is per user, which couples hoster's systemd setup to that user's session (ADR-0023).
- Compat-API gaps (e.g. some podman-specific options) may require targeted libpod calls later.
