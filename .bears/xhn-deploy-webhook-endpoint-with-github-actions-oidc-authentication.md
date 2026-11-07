---
id: xhn
title: Deploy webhook endpoint with GitHub Actions OIDC authentication
status: open
priority: P1
created: "2026-11-07T14:45:17.329258378Z"
updated: "2026-11-07T21:12:34.686583057Z"
tags:
  - web
  - security
depends_on:
  - mzr
  - uma
  - j7u
  - "8w4"
parent: hxh
---

## Summary
`POST /hooks/deploy` on the Axum app per ADR-0047 (OIDC-only authentication) and ADR-0042 (payload, multi-service deploys, digest ownership). No HMAC, no shared secrets.

## Acceptance Criteria
- [ ] `Authorization: Bearer <JWT>` required; body size-limited (64 KiB) and parsed only after the token verifies
- [ ] JWT verification: RS256 against GitHub JWKS (`https://token.actions.githubusercontent.com/.well-known/jwks`), cached, refreshed on unknown `kid` (rate-limited to once a minute); `iss` = `https://token.actions.githubusercontent.com`; `aud` = configured audience (default `https://<admin_host>`, new config key per ADR-0024); `exp`/`nbf`/`iat` with 60 s skew
- [ ] `jti` single-use: stored in SQLite until `exp`; replays rejected
- [ ] Authorization against the stack `[deploy]` rule: `repository_id` (required), optional `refs` globs, `environment`, `job_workflow_ref`; stacks without `[deploy]` reject webhook deploys
- [ ] Payload: stack, images map (service → digest), optional tag; unknown services rejected; digest format validated; reserved `stack_file` rejected
- [ ] Each digest must belong to the service’s configured repo (resolved via the registry client)
- [ ] Provenance stored on the deployment from verified claims (repository, sha, ref, run_id, actor)
- [ ] Rejected attempts (with claims, never the raw token) recorded per stack and shown on the stack page
- [ ] Responses: 200 noop / 202 accepted with deployment id / 401 bad token / 403 rule mismatch / 4xx payload errors — no sensitive detail in bodies

## Implementation Notes
- `jsonwebtoken` (or equivalent) with explicit algorithm allow-list `[RS256]`; never accept `alg: none` or HS*
- JWKS fetch via reqwest + rustls (ring, ADR-0037); fetch failure ⇒ reject, never accept
- Glob matching for `refs`: `*` within a segment, `**` across segments

## Edge Cases & Considerations
- Expired / not-yet-valid tokens, wrong audience, wrong issuer, unknown `kid`, replayed `jti`, oversized body, multiple services in one payload, stack without `[deploy]`

## Testing
- Unit tests with locally generated RSA keys and a mock JWKS server (wiremock) covering every rejection path
- Test vector matching the CI snippet in spec.md §5.2 (claims shape copied from GitHub docs)
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0047, ADR-0042, ADR-0004
- stack-format.md §2.5