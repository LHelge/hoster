---
id: gqp
title: "Decide: when an applied-but-not-redeployed stack revision takes effect (OQ-32)"
status: done
priority: P1
created: "2026-11-07T21:12:34.597478474Z"
updated: "2026-11-07T21:39:44.708920701Z"
tags:
  - decision
  - orchestrator
parent: ame
---

## Summary
Architecture decision gate (project owner decides, agent drafts). spec.md §6 and ADR-0029 say config changes redeploy only when confirmed in the UI, but the idempotency key is (digest, config hash), so a webhook carrying an unchanged digest after an "apply without redeploy" would deploy the pending config. Define the rule before `8w4` wires idempotency.

## Acceptance Criteria
- [ ] Owner has chosen an option in docs/open-questions.md OQ-32
- [ ] Decision recorded as a new ADR (refining ADR-0029); spec.md §6 and ADR-0029's wording aligned
- [ ] `8w4` and `3g2` bodies updated to match

## Implementation Notes
- Documentation-only; no code. Do not mark done without explicit owner sign-off

## References
- docs/open-questions.md OQ-32
- ADR-0029, ADR-0005