# ADR-0008: Embed Pingora as the reverse proxy, on its own thread(s), controlled via channels and ArcSwap

- Status: Accepted; client connections capped in total and per address since ADR-0065
- Date: 2026-11-07

## Context

Every deploy changes routing. The proxy needs to:

- pick up those changes atomically, without reloads
- report when an old upstream has drained
- terminate TLS with certificates that hoster obtains

The orchestrator and the proxy share a lot of state: routes, upstreams, certificates and ACME
challenges.

## Decision

- **Embed the proxy.** Pingora (verified 0.9.1) runs inside the hoster process. Its `Server::run`
  runs on a dedicated `std::thread` and owns its own tokio runtimes, separate from the
  orchestrator's runtime.
- **Shared state.** The orchestrator publishes immutable snapshots through `ArcSwap`:
  `RouteTable`, `CertMap` and `ChallengeMap`.
- **Control.** Lifecycle goes over channels: a custom `ShutdownSignalWatch` carries the shutdown
  signal, and `watch_execution_phase()` reports readiness.
- **Draining.** It is observed through atomic in-flight counters on shared `Upstream` objects.

## Alternatives considered

- **A Traefik or Caddy sidecar.** Rejected:
  - Configuration would go through files or an admin API. Either one is a second source of truth
    that has to be kept in sync, and every swap would cross a process boundary.
  - Swaps would not be atomic from hoster's point of view. Caddy's admin API and Traefik's
    providers each apply config on their own schedule, with their own semantics.
  - Draining would be invisible: hoster could not tell when the old upstream is idle.
  - Both bring their own ACME implementations. Either hoster gives up control of certificates, or
    two ACME clients compete for :80.
  - It is one more process to supervise, upgrade and secure. In a container it also runs into
    ADR-0001.
- **nginx with reloads.** Same problems as a sidecar, and reloads add per-worker config skew.
- **A hand-rolled hyper proxy.** Possible, but Pingora already provides correct HTTP/1 and HTTP/2
  proxying, connection pooling, timeouts and graceful shutdown, and is production-proven.
- **Pingora on the orchestrator's runtime.** Pingora wants to own its runtimes through
  `Server::run`. A separate thread also isolates proxy latency from orchestrator work.

## Consequences

- One binary, one source of truth, atomic swaps (ADR-0017).
- A proxy crash is a hoster crash. Both are supervised by systemd.
- Pingora's defaults must be overridden in code. `grace_period_seconds` defaults to 300 s, and
  `run_forever` exits the process.
- hoster is tied to Pingora's release cadence and API churn. The version is pinned and upgraded
  deliberately.
