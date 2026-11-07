---
id: mzr
title: "Decide: webhook exposure/payload/secrets, replay protection, poller semantics (OQ-11, OQ-12, OQ-13)"
status: done
priority: P1
created: "2026-11-07T14:45:17.324222823Z"
updated: "2026-11-07T19:27:48.492401800Z"
tags:
  - decision
parent: hxh
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-11, OQ-12, OQ-13 before the dependent implementation tasks start. Blocks webhook and poller tasks.

## Acceptance Criteria
- [ ] Owner has chosen an option for each of: OQ-11, OQ-12, OQ-13 (docs/open-questions.md)
- [ ] Each decision is recorded as a new ADR in docs/adr/ (context, alternatives, consequences) and listed in docs/adr/README.md
- [ ] The resolved items are removed from or marked resolved in docs/open-questions.md, and spec.md / architecture.md / stack-format.md are updated where they referenced them as tentative
- [ ] Bodies of the downstream tasks in this milestone are updated if the decision differs from the tentative recommendation

## Implementation Notes
- An agent may prepare a short comparison and a draft ADR, but the choice is the owner's — do not mark done without explicit owner sign-off
- Documentation-only; no code

## References
- docs/open-questions.md
- docs/architecture.md