# ADR-0017: Zero-downtime deploys: start new, health-check, atomically swap, drain, remove old

- Status: Accepted
- Date: 2026-11-07

## Context

Deploys happen on every release. Users must not see errors or dropped connections while a deploy
runs.

## Decision

For each changed routed service:

1. Start a new container (generation *n+1*) next to the old one, on a freshly allocated loopback
   port.
2. Health-check it.
3. Atomically swap the upstream by publishing a new `RouteTable` through `ArcSwap`.
4. Drain the old upstream: wait for its in-flight counter to reach zero, bounded by a drain
   timeout.
5. Stop and remove the old container, and release its port.

If any step before the swap fails, the new container is removed and the old one keeps serving.
The durable "current generation" pointer is the commit point (architecture §5).

## Alternatives considered

- **Stop old, then start new (recreate)**: guaranteed downtime. Kept only as an opt-in strategy
  for services that cannot run twice (ADR-0039).
- **Blue/green with two fixed ports**: forces port reuse and makes stale pooled connections
  ambiguous. Fresh ports are simpler.
- **Health checks inside the proxy (passive)**: detect failures only after users hit them.
  Active checks before the swap are required.
- **Load-balancing old and new together (canary)**: not needed for a single-instance service.
  Could come later.

## Consequences

- Peak resource use is briefly doubled per service during a deploy.
- Apps must tolerate two instances running at once (shared DB migrations and the like are the
  app's concern, per ADR-0018).
- Long-lived connections (SSE, WebSockets) are cut at the drain timeout.
- Requires health check definitions (ADR-0038).
