---
id: ej8
title: "Decide: log access and metrics/alerting (OQ-04, OQ-26)"
status: done
priority: P2
created: "2026-11-07T14:45:17.354252522Z"
updated: "2026-11-07T19:32:45.191588639Z"
tags:
  - decision
parent: "6gn"
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-04, OQ-26 before the dependent implementation tasks start. Blocks M8 implementation tasks.

## Acceptance Criteria
- [ ] Owner has chosen an option for each of: OQ-04, OQ-26 (docs/open-questions.md)
- [ ] Each decision is recorded as a new ADR in docs/adr/ (context, alternatives, consequences) and listed in docs/adr/README.md
- [ ] The resolved items are removed from or marked resolved in docs/open-questions.md, and spec.md / architecture.md / stack-format.md are updated where they referenced them as tentative
- [ ] Bodies of the downstream tasks in this milestone are updated if the decision differs from the tentative recommendation

## Implementation Notes
- An agent may prepare a short comparison and a draft ADR, but the choice is the owner's — do not mark done without explicit owner sign-off
- Documentation-only; no code

## References
- docs/open-questions.md
- docs/architecture.md