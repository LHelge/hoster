---
id: "9ra"
title: In-process statistics for the web UI (no metrics endpoint)
status: done
priority: P2
created: "2026-11-07T14:45:17.364354719Z"
updated: "2026-11-08T02:32:57.232678280Z"
tags:
  - observability
depends_on:
  - "7na"
  - a9y
  - ej8
parent: "6gn"
attempts: 1
---

## Summary
In-process statistics for the web UI per ADR-0045. There is no metrics endpoint and no alerting in v1.

## Acceptance Criteria
- [ ] Per-route request counters by status class, rolling-window latency (p50/p95), upstream errors, in-flight gauge, held in memory
- [ ] Deploy outcomes and durations read from SQLite; reconcile restarts and CrashLooping state; certificate not_after
- [ ] A `Stats` handle the Axum handlers read from; nothing exported over HTTP besides the authenticated UI

## Implementation Notes
- Keep the hot path cheap: pre-allocated atomic counters per route, updated in `logging`

## Testing
- Unit tests for counters and rolling-window percentiles
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0045