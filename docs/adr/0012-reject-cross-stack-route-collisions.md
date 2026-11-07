# ADR-0012: Reject a deploy whose (host, prefix) collides with another stack's route

- Status: Accepted
- Date: 2026-11-07

## Context

Stacks are independent. Without an ownership rule, deploying stack B could silently take over
stack A's traffic.

## Decision

A (host, normalized prefix) pair is owned by exactly one stack. Validation rejects a deploy or
stack apply that declares a pair owned by another stack, before any container starts. Duplicate
pairs inside one stack are a validation error too. Different prefixes on the same host may belong
to different stacks, e.g. A at `/` and B at `/blog`.

## Alternatives considered

- **Last writer wins**: silent hijacks. Rejected.
- **Host-level ownership** (one stack per host): forbids legitimate splits such as a blog under a
  marketing site. Rejected.
- **Explicit transfer with a force flag**: can be added later as an admin action. Not needed now.

## Consequences

- Moving a route between stacks takes two steps: remove it from A, then add it to B.
- `RouteTable::build` re-checks collisions as a last line of defense.
