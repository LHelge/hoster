---
id: t94
title: "Decide: systemd layout for 80/443 and hoster config (OQ-09, OQ-27)"
status: done
priority: P1
created: "2026-11-07T14:45:17.150580232Z"
updated: "2026-11-07T15:07:54.239390670Z"
tags:
  - decision
parent: "8xd"
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-09, OQ-27 before the dependent implementation tasks start. Blocks the systemd unit, doctor and config tasks.

## Acceptance Criteria
- [ ] Owner has chosen an option for each of: OQ-09, OQ-27 (docs/open-questions.md)
- [ ] Each decision is recorded as a new ADR in docs/adr/ (context, alternatives, consequences) and listed in docs/adr/README.md
- [ ] The resolved items are removed from or marked resolved in docs/open-questions.md, and spec.md / architecture.md / stack-format.md are updated where they referenced them as tentative
- [ ] Bodies of the downstream tasks in this milestone are updated if the decision differs from the tentative recommendation
- [ ] The chosen unit layout specifies: service user, how XDG_RUNTIME_DIR/podman.socket is provided, linger, capability or sysctl, state/config/credential directories

## Implementation Notes
- An agent may prepare a short comparison and a draft ADR, but the choice is the owner's — do not mark done without explicit owner sign-off
- Documentation-only; no code

## References
- docs/open-questions.md
- docs/architecture.md