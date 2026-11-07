---
id: uma
title: Reserved admin-host route from Pingora to the Axum app
status: open
priority: P2
created: "2026-11-07T14:45:17.249544996Z"
updated: "2026-11-07T21:12:34.740654563Z"
tags:
  - proxy
  - web
depends_on:
  - pav
  - "9st"
parent: up4
---

## Summary
Route the configured admin host to the Axum app over its Unix socket (`HttpPeer::new_uds`, verified in pingora-core 0.9.1) through the `Target::Unix` variant of `RouteEntry` (`whw`), reserved so no stack can claim it.

## Acceptance Criteria
- [ ] `RouteTable::build` injects a built-in entry for `admin_host` with `Target::Unix(<state dir>/admin.sock)`; the collision check treats the host as reserved
- [ ] UI and `/hooks/*` reachable through the proxy
- [ ] Forwarded headers available to Axum for correct absolute URLs (OAuth callback) and for the `Origin` CSRF check
- [ ] The admin route's read/idle timeouts are long enough for SSE streams (deploy progress, logs) — no fixed 60 s read timeout on this route

## Testing
- Integration test through the proxy to the Axum `/healthz`; SSE endpoint stays open > default read timeout
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0031, ADR-0042, ADR-0044