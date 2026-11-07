---
id: v3m
title: "Decide: orchestrator as actor + Command enum, or shared handle with async methods (OQ-33)"
status: done
priority: P1
created: "2026-11-07T21:12:34.619136334Z"
updated: "2026-11-07T21:44:41.401442663Z"
tags:
  - decision
  - orchestrator
parent: ame
---

## Summary
Architecture decision gate (project owner decides, agent drafts). architecture.md describes the orchestrator as an actor fed by a bounded `Command` channel with oneshot replies. The same single-writer invariant can be had with an `Orchestrator` handle whose async methods take a short lock on desired state and spawn per-stack deploy tasks, without the enum/reply duplication. No ADR currently fixes this; ADR-0008 covers the proxy boundary and ADR-0035 #1 only fixes who performs the swap. Resolve before `8w4`.

## Acceptance Criteria
- [ ] Owner has chosen an option in docs/open-questions.md OQ-33
- [ ] Decision recorded as a new ADR; docs/architecture.md §2.1, §3.3 and the bodies of `8w4`, `3g2`, `pav`, `xhn` updated to the chosen vocabulary

## Implementation Notes
- Documentation-only; no code. Do not mark done without explicit owner sign-off

## References
- docs/open-questions.md OQ-33
- ADR-0008, ADR-0035