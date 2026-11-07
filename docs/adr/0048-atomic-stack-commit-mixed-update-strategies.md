# ADR-0048: A deployment commits the whole stack at once; stop-first services are replaced after start-first services are healthy

- Status: Accepted (resolves OQ-30; supersedes the "Mixed stacks" ordering of ADR-0039)
- Date: 2026-11-07

## Context

ADR-0039 ordered a mixed deployment as "stop-first services first, then start-first services,
each following its own sequence". A stop-first service therefore committed (old stopped, new
swapped in, old removed) before any start-first service had passed its health check. If a
start-first service then failed, the stack was left with some services at the new generation and
others at the old one, a state the deploy state machine (architecture §5) did not describe and
from which the only way back was a manual rollback.

A deployment should be one atomic change to a stack: either every changed service is at the new
generation, or none is.

## Decision

Within one deployment, the engine proceeds in this order:

1. **Start-first services:** pull, create and start the new containers on fresh ports
   (`Starting`), then probe them (`HealthChecking`). Nothing is swapped yet.
2. **Stop-first services** (`Replacing`), one at a time: publish a `RouteTable` in which that
   service's routes are `Unavailable` (503 + `Retry-After`, ADR-0032), stop the old container
   (not remove), start the new one, probe it.
3. **Commit** (`Switching`): one `RouteTable` publish that points every changed service at its new
   upstream, and one transaction that writes all per-service current pointers.
4. **Drain and clean up** (`Draining`, `CleaningUp`) as in ADR-0017. Stopped stop-first
   containers are removed here.

**Failure before step 3** leads to `Aborting`: stopped stop-first containers are started again and
their routes restored, every new container is removed, reserved ports are released. The stack is
exactly as it was. **Failure in step 3 itself** cannot happen once the table is in memory; a
persist failure is handled as in architecture §5.1.

A deployment with no stop-first changes passes through `Replacing` without effects. A deployment
with only stop-first changes has an empty `Starting`/`HealthChecking`.

## Alternatives considered

- **Keep ADR-0039's order and define the half-applied outcome** (`Failed (partially applied)`):
  less change to the record, but a deployment stops being an atomic unit and the UI, rollback and
  crash recovery all need a story for "some services moved".
- **Reject deployments that change both kinds at once:** explicit, but forces two deploys for the
  common case of bumping a storage service and its API together.

## Consequences

- The per-service pointers of a stack always describe one consistent deployment; crash recovery
  (architecture §5.4) needs no partial case.
- The stop-first gap is the same length as under ADR-0039 (stop → start → health). It now starts
  only after the start-first services are already healthy, so a broken start-first image never
  causes a stop-first outage.
- Peak resource use during a deploy is unchanged: start-first services run twice, stop-first
  services once.
- `Replacing` is a new state in the deployment state machine and is shown in the UI.
- Internal callers of a stop-first service inside the stack see it unavailable during its gap,
  as before; the stack file still has to opt into `stop-first` explicitly (ADR-0039).
