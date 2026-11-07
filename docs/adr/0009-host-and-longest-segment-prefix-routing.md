# ADR-0009: Route on host plus longest path prefix, on segment boundaries

- Status: Accepted
- Date: 2026-11-07

## Context

A common layout is a frontend at `/` and an API at `/api` on the same host. Prefix matching must
be predictable, and `/api` must not capture `/apiary`.

## Decision

- **Host.** Match the request host exactly, after normalization (lowercase, port stripped).
- **Path.** Among that host's routes, pick the longest prefix that matches on whole path segments.
  `/api` matches `/api`, `/api/` and `/api/x`, but not `/apix`. `/` matches everything.

## Alternatives considered

- **Raw string prefix**: `/api` would capture `/apiary`. Rejected.
- **First match in declaration order**: depends on file order and on how stacks interleave.
  Rejected.
- **Regex or glob routes**: expressive, but ambiguous to validate for collisions and slower.
  Rejected for now.
- **Header or method based routing**: no current need.

## Consequences

- Collision detection reduces to equality of (host, normalized segments) (ADR-0012).
- Lookup is a hash lookup on the host plus a short scan of that host's routes, sorted by length.
- Wildcard hosts are out until DNS-01 exists (ADR-0016).
