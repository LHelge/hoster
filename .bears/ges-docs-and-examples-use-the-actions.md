---
id: ges
title: Docs and examples use the actions
status: done
priority: P2
created: "2026-11-13T10:04:47.592756Z"
updated: "2026-11-13T10:12:45.563768Z"
tags:
  - docs
depends_on:
  - "9yu"
parent: "8kz"
---

## Summary
Point app repositories at the actions instead of copied shell.

## Acceptance Criteria
- [ ] `examples/github-actions/release.yml` and `examples/static-site/release.yml` deploy with `LHelge/hoster/actions/deploy@<release>`
- [ ] `site/content/wiki/deploying-from-ci.md`: §2 uses the deploy action (inputs and outputs listed), §2.6 the stack-check action; the raw protocol (token request, body, curl) stays in a short "without the action" section for other CI systems
- [ ] `site/content/wiki/static-sites.md` and any other page with the curl step follow; `aphid build` passes
- [ ] README "Cutting a release" notes that the actions are versioned by the release tags

## References
- `site/content/wiki/deploying-from-ci.md`, `site/content/wiki/static-sites.md`, `examples/`, README