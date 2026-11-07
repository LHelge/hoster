---
id: tuv
title: GitHub OAuth login, admin handle check, SQLite sessions, CSRF
status: open
priority: P1
created: "2026-11-07T14:45:17.222053187Z"
updated: "2026-11-07T15:17:39.566434938Z"
tags:
  - web
  - security
depends_on:
  - "9st"
  - h3a
parent: ame
---

## Summary
Authenticate the admin with GitHub OAuth and restrict access to the account named by `HOSTER_ADMIN_GITHUB`, per ADR-0021 and ADR-0031.

## Acceptance Criteria
- [ ] Authorization-code flow with `state` + PKCE via `oauth2` 5.x; callback `/auth/github/callback`; client id/secret from systemd credential or config-referenced file
- [ ] Login accepted only if the authenticated GitHub `login` equals `HOSTER_ADMIN_GITHUB` (case-insensitive); log the numeric id on each login
- [ ] Server-side sessions in SQLite (ADR-0022; `tower-sessions` with a sqlx-0.9-compatible store, or a small in-house store), cookie `__Host-hoster_session`, Secure, HttpOnly, SameSite=Lax, 12 h absolute + idle timeout, logout, 'log out everywhere'
- [ ] CSRF: Origin/Sec-Fetch-Site check on all non-GET requests plus per-session token sent via `hx-headers`
- [ ] All pages except /healthz, /auth/*, /hooks/* require a session

## Implementation Notes
- Never log tokens or codes
- Use reqwest with rustls (ring provider, ADR-0037)

## Edge Cases & Considerations
- Handle comparison is case-insensitive; renaming the admin account requires updating HOSTER_ADMIN_GITHUB (accepted risk, ADR-0031)
- GitHub down → login fails gracefully with a clear message (no break-glass, ADR-0031)

## Testing
- OAuth flow tested against a mock GitHub (wiremock): success, wrong user, bad state, replayed code
- CSRF rejection tests
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0021
- ADR-0031