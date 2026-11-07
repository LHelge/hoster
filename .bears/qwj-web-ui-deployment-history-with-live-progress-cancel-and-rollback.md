---
id: qwj
title: "Web UI: deployment history with live progress, cancel and rollback"
status: open
priority: P2
created: "2026-11-07T14:45:17.319533924Z"
updated: "2026-11-07T14:45:17.319533924Z"
tags:
  - web
depends_on:
  - shg
  - yzq
  - "25t"
  - "3g2"
parent: mdd
---

## Summary
Deployment pages driven by the actor's `ui_events` broadcast (architecture §3.3 #9).

## Acceptance Criteria
- [ ] History per stack with state, source, digests, duration, failure reason
- [ ] Live progress via HTMX SSE extension (or polling fallback) during an active deploy
- [ ] Cancel (before Switching) and 'redeploy this one' (rollback) actions with CSRF protection

## Testing
- Handler tests; SSE stream test
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0021