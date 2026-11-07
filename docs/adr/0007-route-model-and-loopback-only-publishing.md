# ADR-0007: Route model, and only routed services publish ports — on 127.0.0.1, allocated by hoster

- Status: Accepted
- Date: 2026-11-07

## Context

The proxy must reach containers. In rootless podman, container IPs are inside a user network
namespace and not reachable from the host, so published ports are required. Publishing on
`0.0.0.0` would bypass the proxy entirely.

## Decision

- A service declares **zero or more routes**, each: `host`, optional path `prefix`, `strip_prefix`
  flag, container `port` (plus per-route timeouts, ADR-0011).
- Only services with at least one route publish ports, one host port per distinct container port,
  bound to **`127.0.0.1`** only, on a port **hoster allocates** from a configured range.
- Nothing else is reachable from outside.

Verified with podman 6.1.4: a `127.0.0.1`-bound publish is served by `rootlessport` on loopback
and refused on the public interface.

## Alternatives considered

- **User-chosen host ports**: collisions between stacks, and zero-downtime deploys need two
  different host ports at once for the same service. Rejected.
- **Publishing on 0.0.0.0 + firewall**: one firewall mistake exposes everything. Rejected.
- **Unix sockets per upstream** (bind-mounted socket dir): avoids TCP ports but requires apps to
  listen on sockets — app-specific. Rejected.
- **Proxy joins each stack network** (proxy in a container): contradicts ADR-0001.

## Consequences

- hoster owns a port allocator with persistence (range and policy: ADR-0026).
- Any local process on the host can reach `127.0.0.1:<port>`; acceptable on a single-tenant VPS.
- Each new generation gets a fresh port, which also keeps Pingora's upstream connection pools for
  old and new containers separate (helps draining, ADR-0017).
