# ADR-0019: Apps get no podman socket and no nested container access

- Status: Accepted; podman's own init binary is bind-mounted for `init` since ADR-0059
- Date: 2026-11-07

## Context

Access to a container runtime socket is equivalent to full control of every container, and
usually of the host user. Some apps (CI runners, Portainer-like tools, Docker-in-Docker builds)
ask for it.

## Decision

hoster never mounts the podman (or any container runtime) socket into a container. It never runs
containers `--privileged`, with host networking, with host PID/IPC namespaces, or with bind mounts
outside their declared volumes. The stack format has no field that can express any of these.

## Alternatives considered

- **Giving apps a podman socket** (opt-in per service). Rejected:
  - Any compromise of that app becomes a compromise of every stack, of hoster's own state (through
    containers that mount the state directory), and of the hoster user. Rootless podman limits
    this to one user, but that user runs everything.
  - A socket proxy that filters API calls (in the style of docker-socket-proxy) is difficult to
    make airtight. Container create alone can mount arbitrary host paths.
  - It would let apps bypass hoster's routing and port policy (ADR-0007).
- **Nested podman in a dedicated container** (podman-in-podman): needs relaxed seccomp, user
  namespace and device settings. A large attack surface for a non-goal.

## Consequences

- Workloads that need to run containers (CI runners and the like) are out of scope for hoster.
- The stack schema stays deliberately narrow. Review it against this ADR whenever a field is
  added.
