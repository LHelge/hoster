---
id: pzp
title: Notification provider trait and the Resend provider
status: open
priority: P3
created: "2026-11-09T23:22:59.512324730Z"
updated: "2026-11-09T23:22:59.512324730Z"
tags:
  - notify
  - third-party
depends_on:
  - rja
parent: c5s
---

## Summary
A new `src/notify/` module with the provider trait from ADR-0058 and its first implementation, Resend. The owner expects Mailjet and similar HTTP mail APIs next, before SMTP, so the trait must not assume a Resend-shaped request. It is for mail only; SMS and chat are out of scope.

## Acceptance Criteria
- [ ] A provider trait with one delivery method taking a message (sender, recipient, subject, plain text, HTML, idempotency key), returning a typed error that separates retryable from permanent failures.
- [ ] A typed provider enum (the value stored in the settings) that builds the provider from its settings and API key. Adding a provider means one new variant and one new type, with no change to callers.
- [ ] The Resend provider posts to Resend's send-email endpoint with the existing `reqwest` client and rustls setup, with a request timeout, a `User-Agent: hoster/<version>` header (Resend answers 403 without one) and the message's key as `Idempotency-Key`. No new HTTP or mail dependency.
- [ ] Error mapping: per ADR-0058 §4. Network errors, timeouts, 5xx and 429 `rate_limit_exceeded` are retryable (honour `retry-after` when present). 401 and 403 (invalid or suspended key, sender domain not verified, test sender to a foreign address), 4xx validation errors and 429 `daily_quota_exceeded` / `monthly_quota_exceeded` are permanent and carry the provider's message for the UI.
- [ ] The API key is a `SecretValue` and never appears in logs, errors or `Debug` output.
- [ ] The base URL is injectable so tests run against a local server.

## Implementation Notes
- ADR-0058 §4 has the request and status codes as checked on 2026-11-10. The JSON shape of Resend's error body was not confirmed then: check the API reference (https://resend.com/docs/api-reference/errors) before parsing it, and fall back to the status code when the body doesn't parse.
- Keep the module deep: the public surface is the trait, the message type, the provider enum and the error. The Resend client is private.
- `src/registry/client.rs` shows how the project builds and tests a `reqwest` client against a fake server.

## Edge Cases
- A response that isn't JSON (a proxy error page) still maps to a retryable or permanent error by status.
- A timeout is retryable.

## Testing
- Tests against a local HTTP server: success, 401, 403 unverified domain, 422, 429 rate limit with `retry-after`, 429 daily quota, 500, timeout, and that the request carries the expected fields.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the tests pass.

## References
- ADR-0058, ADR-0029
- `src/registry/client.rs`, `src/secrets/mod.rs` (`SecretValue`)