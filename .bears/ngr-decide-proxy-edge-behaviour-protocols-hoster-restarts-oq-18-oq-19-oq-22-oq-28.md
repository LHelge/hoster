---
id: ngr
title: "Decide: proxy edge behaviour, protocols, hoster restarts (OQ-18, OQ-19, OQ-22, OQ-28)"
status: done
priority: P1
created: "2026-11-07T14:45:17.229638334Z"
updated: "2026-11-07T15:57:16.031904151Z"
tags:
  - decision
  - proxy
parent: up4
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-18, OQ-19, OQ-22, OQ-28 before the dependent implementation tasks start. Blocks the ProxyHttp implementation and the TLS listener.

## Acceptance Criteria
- [ ] Owner has chosen an option for each of: OQ-18, OQ-19, OQ-22, OQ-28 (docs/open-questions.md)
- [ ] Each decision is recorded as a new ADR in docs/adr/ (context, alternatives, consequences) and listed in docs/adr/README.md
- [ ] The resolved items are removed from or marked resolved in docs/open-questions.md, and spec.md / architecture.md / stack-format.md are updated where they referenced them as tentative
- [ ] Bodies of the downstream tasks in this milestone are updated if the decision differs from the tentative recommendation

## Implementation Notes
- An agent may prepare a short comparison and a draft ADR, but the choice is the owner's — do not mark done without explicit owner sign-off
- Documentation-only; no code

## References
- docs/open-questions.md
- docs/architecture.md