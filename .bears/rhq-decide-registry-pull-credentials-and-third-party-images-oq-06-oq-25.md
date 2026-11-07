---
id: rhq
title: "Decide: registry pull credentials and third-party images (OQ-06, OQ-25)"
status: done
priority: P1
created: "2026-11-07T14:45:17.169656874Z"
updated: "2026-11-07T15:10:12.208177024Z"
tags:
  - decision
parent: "5ch"
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-06, OQ-25 before the dependent implementation tasks start. Blocks the pull and ghcr client tasks.

## Acceptance Criteria
- [ ] Owner has chosen an option for each of: OQ-06, OQ-25 (docs/open-questions.md)
- [ ] Each decision is recorded as a new ADR in docs/adr/ (context, alternatives, consequences) and listed in docs/adr/README.md
- [ ] The resolved items are removed from or marked resolved in docs/open-questions.md, and spec.md / architecture.md / stack-format.md are updated where they referenced them as tentative
- [ ] Bodies of the downstream tasks in this milestone are updated if the decision differs from the tentative recommendation
- [ ] Verify whether ghcr accepts fine-grained PATs today (test pull) and record the result in the ADR

## Implementation Notes
- An agent may prepare a short comparison and a draft ADR, but the choice is the owner's — do not mark done without explicit owner sign-off
- Documentation-only; no code

## References
- docs/open-questions.md
- docs/architecture.md