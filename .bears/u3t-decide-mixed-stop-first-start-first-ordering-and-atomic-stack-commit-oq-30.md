---
id: u3t
title: "Decide: mixed stop-first/start-first ordering and atomic stack commit (OQ-30)"
status: done
priority: P1
created: "2026-11-07T21:12:34.568821499Z"
updated: "2026-11-07T21:35:20.264866147Z"
tags:
  - decision
  - orchestrator
parent: mdd
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-30 before `shg` and `v2h` start: in a deployment that changes both stop-first and start-first services, ADR-0039 runs stop-first services first, which commits them before the start-first services have passed health. A later health failure leaves the stack half-applied.

## Acceptance Criteria
- [ ] Owner has chosen an option in docs/open-questions.md OQ-30 (keep ADR-0039 order and define the half-applied outcome, or reverse the order so the whole stack commits in one swap)
- [ ] Decision recorded as a new ADR; if the order changes, it supersedes the "Mixed stacks" part of ADR-0039
- [ ] docs/architecture.md §5 and the bodies of `shg` and `v2h` updated to match

## Implementation Notes
- Documentation-only; no code. Do not mark done without explicit owner sign-off

## References
- docs/open-questions.md OQ-30
- ADR-0039, ADR-0017