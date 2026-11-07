---
id: hft
title: "Decide: poller bootstrap, advisory DNS check, health default path, PORT injection, host redirects (OQ-34)"
status: done
priority: P2
created: "2026-11-07T21:12:34.629561142Z"
updated: "2026-11-07T21:50:54.320669961Z"
tags:
  - decision
parent: ame
---

## Summary
Architecture decision gate (project owner decides, agent drafts) for five small amendments found in review, grouped to save ceremony:

1. ADR-0043 records a tag's first sighting without deploying, so the poller can never bootstrap a never-deployed service.
2. ADR-0036 defers issuance when A/AAAA records don't point at "this server", but nothing defines where hoster learns its own addresses, and a proxying CDN makes the check wrong while HTTP-01 still works.
3. The default health probe is `GET /`, which 404s for a prefix-mounted service that keeps its prefix.
4. Inject `PORT=<container port>` for routed services with one distinct port unless `env` sets it (12-factor convenience).
5. Host-level redirects (`www.example.com` → `https://example.com`) as a stack-level `[[redirects]]` entry served from the RouteTable as `Target::Redirect`.

## Acceptance Criteria
- [ ] Owner has chosen an option for each item in docs/open-questions.md OQ-34
- [ ] Decisions recorded in one ADR (amending ADR-0043, ADR-0036, ADR-0038, ADR-0032 and stack-format.md)
- [ ] Bodies of `68p`, `a9y`, `cuh`, `gnz`, `whw` (Redirect target), `wks` (redirect response), `4tj` (redirect hosts in collision check), `8eh` (schema) and docs/stack-format.md updated

## Implementation Notes
- Documentation-only; no code. Do not mark done without explicit owner sign-off

## References
- docs/open-questions.md OQ-34