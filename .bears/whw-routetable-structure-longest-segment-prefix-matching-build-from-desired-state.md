---
id: whw
title: "RouteTable: structure, longest-segment-prefix matching, build from desired state"
status: open
priority: P1
created: "2026-11-07T14:45:17.233547428Z"
updated: "2026-11-07T21:32:51.727668772Z"
tags:
  - routing
depends_on:
  - "4tj"
  - hft
parent: up4
---

## Summary
Pure routing module per architecture §6: immutable RouteTable built from desired state, with matching semantics from ADR-0009. The table must carry **three target kinds** from the start, because `uma` (admin host → Unix socket) and `v2h` (stop-first gap → 503) need them: do not model a route as "always a loopback upstream".

## Acceptance Criteria
- [ ] `RouteTable::build(&DesiredState, reserved) -> Result<RouteTable, CollisionError>` (re-runs collision check)
- [ ] `lookup(host, path) -> Option<&RouteEntry>`: host normalized (lowercase, strip port, trailing dot), longest prefix on segment boundaries
- [ ] `RouteEntry.target: Target` with variants `Loopback(Arc<Upstream>)` (127.0.0.1:<port>, inflight counter), `Unix(Arc<UnixUpstream>)` (admin app socket path), `Unavailable { retry_after: Duration }` (stop-first gap)
- [ ] `Upstream` with `addr`, ids and `inflight: AtomicU64` held as `Arc<Upstream>` (architecture §3.2)
- [ ] Strip-prefix path computation helper: query preserved, empty → '/'
- [ ] `version` incremented per build; diff summary helper for logging

## Edge Cases & Considerations
- /api vs /apiary vs /api/ vs /api?x
- percent-encoded slashes are not segment separators
- routes for same host across stacks
- the reserved admin host entry is injected by `build`, not by any stack

## Testing
- Table-driven + proptest (lookup result is always the longest matching prefix)
- Criterion micro-bench for lookup (sanity, not a gate)
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §6
- ADR-0009, ADR-0010, ADR-0031, ADR-0039