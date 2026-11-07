# ADR-0006: Apps are stacks of containers, each stack on its own podman network

- Status: Accepted
- Date: 2026-11-07

## Context

Real apps are rarely one container: a backend, a static frontend, object storage, a worker.
Services within an app must find each other; apps must not see each other.

## Decision

The unit of deployment is a **stack**: a named set of one or more services. Each stack gets a
dedicated podman network `hoster-<stack>`; services join it with their service name as a network
alias, so they reach each other by name. Stacks share no network.

## Alternatives considered

- **Single container per app**: forces sidecar-less designs or multiple pseudo-apps with manual
  wiring. Rejected.
- **One shared network for everything**: any compromised app can reach every other app's internal
  ports. Rejected.
- **Podman pods** (shared network namespace per stack): services would share `localhost` and port
  space, which collides with the routing model (each routed service publishes its own ports) and
  breaks running two generations side by side during zero-downtime deploys. Rejected.
- **Full docker-compose compatibility**: large surface, semantics that don't map (build, links,
  extends). We take inspiration but define our own format (see stack-format.md).

## Consequences

- During a deploy the old and new generation coexist on the network and share the alias; internal
  callers may hit either during the overlap.
- Cross-stack communication is only via public routes unless we add something (ADR-0046).
