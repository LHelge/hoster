---
id: yvk
title: "Decide: admin UI exposure, sessions, identity, CSRF (OQ-05)"
status: done
priority: P1
created: "2026-11-07T14:45:17.194278634Z"
updated: "2026-11-07T15:17:39.572010639Z"
tags:
  - decision
  - web
parent: ame
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-05 before the dependent implementation tasks start. ADR-0021 already fixes Axum + Askama + HTMX 4 + GitHub OAuth; this resolves the remaining details.

## Acceptance Criteria
- [ ] Owner has chosen an option for each of: OQ-05 (docs/open-questions.md)
- [ ] Each decision is recorded as a new ADR in docs/adr/ (context, alternatives, consequences) and listed in docs/adr/README.md
- [ ] The resolved items are removed from or marked resolved in docs/open-questions.md, and spec.md / architecture.md / stack-format.md are updated where they referenced them as tentative
- [ ] Bodies of the downstream tasks in this milestone are updated if the decision differs from the tentative recommendation
- [ ] Confirms handle→numeric-id pinning, session store, CSRF approach, Unix-socket vs loopback listener, break-glass login link

## Implementation Notes
- An agent may prepare a short comparison and a draft ADR, but the choice is the owner's — do not mark done without explicit owner sign-off
- Documentation-only; no code

## References
- docs/open-questions.md
- docs/architecture.md