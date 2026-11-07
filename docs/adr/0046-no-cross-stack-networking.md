# ADR-0046: No direct cross-stack networking in v1

- Status: Accepted (resolves OQ-23)
- Date: 2026-11-07

## Decision

Stacks are fully isolated on their own networks (ADR-0006). Stack A can reach stack B only through
B's public HTTPS routes, which hairpin through hoster's proxy.

## Alternatives considered

- **Declared links** (`links = ["other-stack"]` creating a shared network): more scope and a
  weaker isolation boundary. Revisit if a real need appears.

## Consequences

- Services that need to be shared between apps, such as a common database, either sit in one
  stack or are exposed through a route on their own hostname.
