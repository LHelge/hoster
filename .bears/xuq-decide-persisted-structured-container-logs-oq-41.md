---
id: xuq
title: "Decide: persisted, structured container logs (OQ-41)"
status: open
priority: P2
created: "2026-11-12T13:13:40.130227673Z"
updated: "2026-11-12T13:13:40.130227673Z"
tags:
  - decision
parent: f8z
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Resolve OQ-41 before the log store epic is broken down. It also fixes what `psq` (stack logs UI) and the edge request log epic (`usf`) read from.

## Acceptance Criteria
- [x] Owner has chosen an option for each numbered question of OQ-41 (docs/open-questions.md): persist or not, where, record shape and parsing, retention and caps, search, backups, the access-log source
- [x] The decision is recorded as ADR-0067 in docs/adr/ (amending ADR-0044 and ADR-0045) and listed in docs/adr/README.md
- [x] OQ-41 is marked resolved in docs/open-questions.md
- [ ] Epic `f8z` is broken down into tasks, and the bodies of `psq` and the edge request log epic (`usf`) are updated if the decision differs from the recommendation

## Implementation Notes
- An agent may prepare a comparison and a draft ADR, but the choice is the owner's. Do not mark done without explicit owner sign-off
- If per-day SQLite files are chosen, check how sqlx pools and reversible migrations (ADR-0052) work for files created at run time before writing the ADR
- Check whether FTS5 is compiled into the sqlx SQLite build if the search question leans that way
- Documentation-only; no code beyond such checks

## References
- docs/open-questions.md (OQ-41)
- ADR-0044, ADR-0045, ADR-0022, ADR-0062, ADR-0030, ADR-0052