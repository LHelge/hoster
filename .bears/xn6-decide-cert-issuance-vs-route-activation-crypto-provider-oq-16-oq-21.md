---
id: xn6
title: "Decide: cert issuance vs route activation, crypto provider (OQ-16, OQ-21)"
status: done
priority: P1
created: "2026-11-07T14:45:17.253629371Z"
updated: "2026-11-07T15:59:28.596627470Z"
tags:
  - decision
  - tls
parent: "52q"
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-16, OQ-21 before the dependent implementation tasks start. Blocks cert store, ACME and TLS listener tasks.

## Acceptance Criteria
- [ ] Owner has chosen an option for each of: OQ-16, OQ-21 (docs/open-questions.md)
- [ ] Each decision is recorded as a new ADR in docs/adr/ (context, alternatives, consequences) and listed in docs/adr/README.md
- [ ] The resolved items are removed from or marked resolved in docs/open-questions.md, and spec.md / architecture.md / stack-format.md are updated where they referenced them as tentative
- [ ] Bodies of the downstream tasks in this milestone are updated if the decision differs from the tentative recommendation

## Implementation Notes
- An agent may prepare a short comparison and a draft ADR, but the choice is the owner's — do not mark done without explicit owner sign-off
- Documentation-only; no code

## References
- docs/open-questions.md
- docs/architecture.md