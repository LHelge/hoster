---
id: mqu
title: OIDC-authenticated deployment status endpoint for CI
status: open
priority: P2
created: "2026-11-14T21:53:52.657811482Z"
updated: "2026-11-14T22:08:24.491553992Z"
tags:
  - webhook
depends_on:
  - msm
parent: aax
---

## Summary
`GET /hooks/deploy/{stack}/deployments/{id}` for the deploy action, as ADR-0070 decides: an OIDC token that the read does not consume, scoped to deployments the token's own workflow run started.

## Acceptance Criteria
- [ ] Route next to `/hooks/deploy` (`src/web/mod.rs`), on the `Webhook` type (`src/web/webhook.rs`), outside the admin session guard
- [ ] ADR-0047's verification (signature, `iss`, `aud`, lifetime with skew) shared with the POST; the read neither checks nor records the `jti`, so the token the deploy used still reads
- [ ] Authorised by `{stack}`'s `[deploy]` rule exactly as a deploy; the deployment must be `{stack}`'s and its provenance must have the token's `repository` and `run_id`
- [ ] `200` JSON: `deployment_id`, `stack`, `state` (spelled as in the 202), `terminal`, `reason` (`Deployment::error`), `created_at`, `finished_at` (RFC 3339 UTC, `null` while running), `url`
- [ ] `401` without detail for a bad token; `403` for a rule failure or a stack without `[deploy]`, **not** recorded as a rejected attempt; one identical `404` for unknown stack, unknown id, another stack's id and another run's deployment
- [ ] Tests in `src/web/webhook/tests.rs`: live, failed with reason, in progress, a token already used for the deploy still reads, unknown id, foreign stack's id, another run's deployment, a UI deployment, rule mismatch, expired token
- [ ] architecture.md §9 (security boundaries) lists the endpoint with the webhook

## References
- ADR-0070, ADR-0047, `src/web/webhook.rs`, `src/web/webhook/tests.rs`, `src/store/types.rs` (`Deployment`, `Provenance`, `DeploymentState::is_terminal`), `docs/architecture.md` §9