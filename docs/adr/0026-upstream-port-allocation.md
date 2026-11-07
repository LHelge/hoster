# ADR-0026: Upstream port allocation: round-robin over a configured range, persisted in SQLite

- Status: Accepted (resolves OQ-20)
- Date: 2026-11-07

## Context

Every routed container publishes its ports on `127.0.0.1` at host ports that hoster chooses
(ADR-0007). Each deploy generation gets fresh ports, because the old and new containers run side
by side (ADR-0017).

## Decision

- **Range.** Configurable in `hoster.toml`, default `20000–29999`.
- **Persistence.** Allocations are stored in SQLite (ADR-0022) as port → (stack, service,
  container port, generation).
- **Policy.** The next free port after the last one handed out, wrapping around. A released port
  is therefore not reused right away.
- **Conflicts.** If podman reports "address in use" because some other host process took the
  port, that port is skipped for 10 minutes and the next one is tried.
- **Exhaustion.** If no port is free, the deploy fails validation (`Rejected`).

## Alternatives considered

- **Lowest free port**: compact, but a released port can be reused immediately.
- **Kernel-chosen port** (`HostPort: 0`): no allocator to maintain, but ports have to be read back
  from inspect, change on recreate, and are harder to reason about.

## Consequences

- The range must not overlap other services on the host, and must stay outside the kernel's
  ephemeral range (`net.ipv4.ip_local_port_range`, typically 32768–60999). `hoster doctor`
  checks this.
- Not reusing ports straight away also keeps Pingora's per-address connection pools for old and
  new containers apart (architecture §5.3).
