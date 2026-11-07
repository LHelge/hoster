# ADR-0020: Single host only

- Status: Accepted
- Date: 2026-11-07

## Context

Multi-host orchestration (scheduling, overlay networks, distributed state, leader election) is
where most of the complexity of Kubernetes, Nomad and Swarm lives. The target here is one VPS.

## Decision

hoster manages exactly one host: the one it runs on. Nothing in the design should assume a second
node.

## Alternatives considered

- **Designing for clustering later** (pluggable scheduler, distributed store): it would cost
  complexity now for a hypothetical future. Rejected.

## Consequences

- The state store can be local (ADR-0022), and the port allocator, proxy and ACME state are all
  in-process.
- High availability is out of scope. A host or hoster outage is an outage.
- Scaling a service to several replicas on the one host is not planned. If it is ever wanted, the
  `RouteTable` design (an upstream list per route) leaves room for it.
