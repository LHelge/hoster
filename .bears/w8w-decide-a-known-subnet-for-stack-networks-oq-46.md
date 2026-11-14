---
id: w8w
title: "Decide: a known subnet for stack networks (OQ-46)"
status: open
priority: P3
created: "2026-11-14T21:53:41.004007812Z"
updated: "2026-11-14T21:53:41.004007812Z"
tags:
  - decision
---

## Summary
Architecture decision gate (project owner decides, agent drafts). Podman picks each stack network's subnet, so an app that trusts forwarded headers only from its proxy must trust all of `10.0.0.0/8`. Decide between a per-stack `[network] subnet`, one hoster-wide pool, or documenting the gateway only (OQ-46). User feedback of 2026-11-14 asked for an ADR before any implementation.

## Acceptance Criteria
- [ ] Owner has chosen an option of OQ-46 (docs/open-questions.md)
- [ ] The decision is recorded as a new ADR (amending ADR-0006) and listed in docs/adr/README.md; a "no" is recorded too
- [ ] OQ-46 is marked resolved
- [ ] Implementation tasks are filed (stack check / config validation, network create, docs), or none on a "no"

## Implementation Notes
- Verify first where hoster's proxied connections actually arrive from inside a stack network (rootless podman port forwarding: pasta vs rootlessport) before writing the ADR; option C depends on it
- Documentation-only; no code. Do not mark done without explicit owner sign-off

## References
- docs/open-questions.md (OQ-46), ADR-0006, ADR-0007, ADR-0046, `src/runtime/podman.rs` (network create), `docs/architecture.md` §8