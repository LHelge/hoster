---
id: pu4
title: "Decide: upstream port allocation policy (OQ-20)"
status: done
priority: P2
created: "2026-11-07T14:45:17.166489343Z"
updated: "2026-11-07T15:11:08.692700294Z"
tags:
  - decision
parent: "5ch"
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-20 before the dependent implementation tasks start. Blocks the port allocator task. (OQ-01 is already resolved: ADR-0022, SQLite via sqlx.)

## Acceptance Criteria
- [ ] Owner has chosen an option for each of: OQ-20 (docs/open-questions.md)
- [ ] Each decision is recorded as a new ADR in docs/adr/ (context, alternatives, consequences) and listed in docs/adr/README.md
- [ ] The resolved items are removed from or marked resolved in docs/open-questions.md, and spec.md / architecture.md / stack-format.md are updated where they referenced them as tentative
- [ ] Bodies of the downstream tasks in this milestone are updated if the decision differs from the tentative recommendation

## Implementation Notes
- An agent may prepare a short comparison and a draft ADR, but the choice is the owner's — do not mark done without explicit owner sign-off
- Documentation-only; no code

## References
- docs/open-questions.md
- docs/architecture.md