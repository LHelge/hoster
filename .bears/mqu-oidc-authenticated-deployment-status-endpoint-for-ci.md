---
id: mqu
title: OIDC-authenticated deployment status endpoint for CI
status: open
priority: P2
created: "2026-11-14T21:53:52.657811482Z"
updated: "2026-11-14T21:53:52.657811482Z"
tags:
  - webhook
depends_on:
  - msm
parent: aax
---

## Summary
A machine-readable read of one deployment for the deploy action, authenticated as the ADR from `msm` decides (OQ-45 recommends `GET /api/deploy/{stack}/deployments/{id}` with an OIDC token that the read does not consume).

## Acceptance Criteria
- [ ] Route next to `/hooks/deploy` (`src/web/mod.rs`), on the `Webhook` type (`src/web/webhook.rs`), outside the admin session guard
- [ ] Token verified as the ADR says; authorised by the stack's `[deploy]` rule and scoped to that stack's deployments (and provenance, if decided)
- [ ] JSON answer: id, stack, state, terminal, reason, timestamps, admin URL
- [ ] Unknown stack, unknown id and another stack's id all answer the same 404; a token failing the rule is refused like the webhook, and listed on the stack page as a rejected attempt if the ADR says so
- [ ] Tests in `src/web/webhook/tests.rs`: live, failed, in progress, unknown id, foreign id, wrong repository, expired token
- [ ] architecture.md §9 (security boundaries) mentions the endpoint

## References
- ADR from `msm`, ADR-0047, `src/web/webhook.rs`, `src/web/webhook/tests.rs`, `src/store/types.rs`, `docs/architecture.md` §9