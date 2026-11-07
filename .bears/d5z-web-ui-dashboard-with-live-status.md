---
id: d5z
title: Web UI dashboard with live status
status: open
priority: P2
created: "2026-11-07T14:45:17.379437087Z"
updated: "2026-11-07T14:45:17.379437087Z"
tags:
  - web
  - observability
depends_on:
  - "9ra"
  - "98v"
  - qwj
  - cuh
parent: "6gn"
---

## Summary
The landing page: every stack/service with digest, health, last deploy, cert expiry; live-updating via HTMX SSE.

## Acceptance Criteria
- [ ] Dashboard renders server-side; partial updates on `ui_events`
- [ ] Warning badges: failing health, crash loop, cert expiring < 14 days, failed last deploy. This is the only alerting surface in v1 (ADR-0045)

## Testing
- Template and handler tests
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- spec.md §5.4
- ADR-0021