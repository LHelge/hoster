---
id: j7u
title: "Decide: GitHub Actions OIDC instead of HMAC for the deploy webhook (OQ-29)"
status: done
priority: P1
created: "2026-11-07T19:44:25.652101499Z"
updated: "2026-11-07T19:59:12.467594114Z"
tags:
  - decision
  - security
parent: hxh
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-29: authenticate the deploy webhook with GitHub Actions OIDC ID tokens instead of (or in addition to) the per-stack HMAC secrets of ADR-0042.

## Acceptance Criteria
- [ ] Owner has chosen A (keep HMAC), B (OIDC only) or C (OIDC + opt-in HMAC fallback) — docs/open-questions.md OQ-29
- [ ] Decision recorded as a new ADR; if B or C, it supersedes the authentication parts of ADR-0042 (ADR-0042 status updated)
- [ ] If B/C: stack-format.md gains the `[deploy]` authorization section (repository_id required; optional refs, environment, job_workflow_ref)
- [ ] spec.md CI example updated (id-token: write, request token with the configured audience, Authorization: Bearer)
- [ ] Bodies of the webhook, webhook-secrets-UI and CI-docs tasks updated (secrets-UI task cancelled if B)

## Implementation Notes
- Documentation-only; no code. Do not mark done without explicit owner sign-off

## References
- docs/open-questions.md OQ-29
- ADR-0042, ADR-0025 (correction)