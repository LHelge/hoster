# ADR-0039: Per-service update strategy: start-first (default) or stop-first

- Status: Accepted (resolves OQ-14); the "Mixed stacks" ordering is superseded by ADR-0048
- Date: 2026-11-07

## Decision

- Each service has `update = "start-first" | "stop-first"`. The default is `start-first`, which
  is the zero-downtime path from ADR-0017.
- **stop-first:**
  1. Stop the old container. It is stopped, not removed.
  2. Start the new container.
  3. Health-check it.
  4. Swap the route.
  5. Remove the old container.
- **If the new container fails** under stop-first, the old container is started again and the
  deployment is `Failed`.
- **During the gap**, the proxy returns **503 with `Retry-After`** for that service's routes
  (ADR-0032).
- **Mixed stacks:** within one deployment, stop-first services go first, then start-first
  services. Each service still follows its own sequence.
- **Validation warns** when a service with volumes uses `start-first`.

## Alternatives considered

- **Infer stop-first from volumes**: too magic, since some volume users (caches) can run twice.
- **Always start-first**: unsafe for databases and anything with exclusive locks.

## Consequences

- Services such as rustfs see a short outage per deploy. The stack file states this explicitly.
