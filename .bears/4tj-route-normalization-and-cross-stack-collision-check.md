---
id: "4tj"
title: Route normalization and cross-stack collision check
status: done
priority: P1
created: "2026-11-07T14:45:17.201012420Z"
updated: "2026-11-07T22:42:49.938129209Z"
tags:
  - stack
  - routing
depends_on:
  - "8eh"
parent: ame
attempts: 1
---

## Summary
Pure function that checks a candidate stack's routes against all other stacks' routes (and reserved routes such as the admin host) per ADR-0012.

## Acceptance Criteria
- [ ] check(candidate, others, reserved) → Ok or list of collisions naming both owners
- [ ] Collision = same normalized host + identical prefix segments; different prefixes on the same host across stacks allowed
- [ ] Duplicate (host, prefix) within one stack rejected
- [ ] Reserved admin host (from config) can never be claimed by a stack

## Implementation Notes
- Reuse NormalizedPrefix; this exact function is also called by RouteTable::build

## Testing
- Unit + property tests (proptest): symmetric, normalization-invariant
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0012
- docs/architecture.md §6.2