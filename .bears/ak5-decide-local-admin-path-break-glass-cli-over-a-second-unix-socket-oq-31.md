---
id: ak5
title: "Decide: local admin path / break-glass CLI over a second Unix socket (OQ-31)"
status: done
priority: P1
created: "2026-11-07T21:12:34.586936303Z"
updated: "2026-11-07T21:37:08.641359455Z"
tags:
  - decision
  - web
  - ops
parent: ame
---

## Summary
Architecture decision gate (project owner decides, agent drafts). ADR-0031 has no break-glass: if GitHub OAuth is down, the operator edits SQLite by hand. A GitHub outage usually takes Actions and OAuth down together, so neither deploys nor rollbacks are possible. The Axum app already listens on a Unix socket; a second local-only socket plus `hoster apply|deploy|rollback` subcommands would be a small addition and would also make M3 and the e2e tests usable without a browser.

## Acceptance Criteria
- [ ] Owner has chosen an option in docs/open-questions.md OQ-31
- [ ] Decision recorded as a new ADR (refining ADR-0021/ADR-0031, not replacing the web UI)
- [ ] If accepted: a task is created under M3 for the local socket + CLI subcommands, and `9st`/`4nb` bodies reference it

## Implementation Notes
- Documentation-only; no code. Do not mark done without explicit owner sign-off

## References
- docs/open-questions.md OQ-31
- ADR-0021, ADR-0031