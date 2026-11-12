---
id: fxc
title: "Decide: a filter language for the Telemetry query box (OQ-43)"
status: open
priority: P3
created: "2026-11-12T19:38:09.681353801Z"
updated: "2026-11-12T19:38:09.681353801Z"
tags:
  - decision
depends_on:
  - ckc
parent: ddf
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Decide whether the Telemetry query box gets a filter language (OQ-43, option B or C) or stays grep beside controls for good (option A). Interim answer of 2026-11-12: A until the tabs have been used.

## Acceptance Criteria
- [ ] The owner has used the Logs tab (and the Traces tab, if it has landed) and names the filters they wanted to type rather than click
- [ ] Owner has chosen an option of OQ-43 (docs/open-questions.md)
- [ ] The decision is recorded as a new ADR in docs/adr/ (amending ADR-0067 §8 if a language is added) and listed in docs/adr/README.md; a "no" is recorded too
- [ ] OQ-43 is marked resolved
- [ ] Epic `ddf` is broken down, or cancelled on a "no"

## Implementation Notes
- An agent may prepare examples of each option against real records and a draft ADR, but the choice is the owner's. Do not mark done without explicit owner sign-off
- Documentation-only; no code

## References
- docs/open-questions.md (OQ-43), ADR-0067 (§8), `docs/design/Telemetry.dc.html`