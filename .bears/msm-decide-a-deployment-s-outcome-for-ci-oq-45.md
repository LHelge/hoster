---
id: msm
title: "Decide: a deployment's outcome for CI (OQ-45)"
status: done
priority: P2
created: "2026-11-14T21:53:46.182890355Z"
updated: "2026-11-14T22:08:24.516224423Z"
tags:
  - decision
parent: aax
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Choose how CI reads a deployment's state: fresh OIDC token per poll, OIDC reads that don't consume the `jti`, or a read capability in the 202 answer (OQ-45); and how far the read is scoped (the stack's `[deploy]` rule, plus same `repository_id` provenance).

## Acceptance Criteria
- [x] Owner has chosen an option of OQ-45 (docs/open-questions.md)
- [x] Recorded as a new ADR amending ADR-0042/ADR-0047 (path, answer JSON, auth, 404 for unknown or foreign ids), listed in docs/adr/README.md
- [x] OQ-45 marked resolved
- [x] The endpoint and action task bodies updated to match

## Implementation Notes
- Owner chose option B (the recommendation) on 2026-11-14; the provenance scope became the same workflow run (`repository` + `run_id`), already stored, instead of `repository_id`. The action re-mints on 401, so the exact token lifetime does not matter
- Check GitHub's OIDC token lifetime and request limits before recommending a per-poll token
- Documentation-only; no code. Do not mark done without explicit owner sign-off

## References
- docs/open-questions.md (OQ-45), ADR-0042, ADR-0047, `src/web/webhook.rs`, `src/store/types.rs` (`DeploymentState::is_terminal`)