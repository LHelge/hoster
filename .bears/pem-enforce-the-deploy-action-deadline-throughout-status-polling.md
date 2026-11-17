---
id: pem
title: Enforce the deploy action deadline throughout status polling
status: open
priority: P2
created: "2026-11-17T23:46:05.815675297Z"
updated: "2026-11-17T23:46:05.815675297Z"
---

## Summary
Poll requests and sleep can exceed the remaining budget, and terminal responses and token refresh can bypass the timeout check.

## Acceptance criteria
Cover a late terminal response, a token refresh near expiry, and a polling interval longer than the remaining timeout. Report timeout consistently while the server deployment continues.

## References
- actions/deploy/deploy.sh
- actions/deploy/test.sh
- ADR-0070