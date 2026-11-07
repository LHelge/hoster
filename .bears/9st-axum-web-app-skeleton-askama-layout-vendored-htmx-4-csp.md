---
id: "9st"
title: "Axum web app skeleton: Askama layout, vendored HTMX 4, CSP"
status: in_progress
priority: P1
created: "2026-11-07T14:45:17.218424594Z"
updated: "2026-11-07T22:47:16.732090310Z"
tags:
  - web
depends_on:
  - hyt
  - yvk
  - ak5
parent: ame
attempts: 1
---

## Summary
Stand up the admin web app (ADR-0021): Axum router in the orchestrator runtime, Askama base layout, vendored HTMX 4 embedded in the binary, strict security headers. Listens on a 0600 Unix socket in the state dir (ADR-0031); a loopback TCP option exists for local development only.

## Acceptance Criteria
- [ ] axum 0.8 router with `/healthz` (unauthenticated) and a placeholder dashboard page
- [ ] Askama base template with nav and a flash-message area; templates compile into the binary
- [ ] htmx 4.0.x file vendored under `assets/` with its version and checksum recorded in `assets/VENDORED.md`; served from `/assets/` with long cache + content hash
- [ ] Security headers: strict CSP (`default-src 'self'`), X-Content-Type-Options, Referrer-Policy, frame-ancestors 'none'
- [ ] Graceful shutdown wired to the root CancellationToken

## Implementation Notes
- Embed assets via `include_bytes!` or `rust-embed`
- Minimal CSS (hand-written or a vendored classless stylesheet) — no JS build step

## Testing
- Router tests with `tower::ServiceExt::oneshot`: CSP header present, asset served with correct content-type
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0021
- ADR-0031