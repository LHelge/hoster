---
id: ap5
title: Docs site built with aphid, released as an nginx image and deployed to hoster.lhelge.se
status: in_progress
priority: P2
created: "2026-11-08T15:36:13.210125125Z"
updated: "2026-11-08T15:36:20.016437599Z"
tags:
  - docs
  - ci
attempts: 1
---

Dogfooding: publish hoster's user documentation as the first app on the owner's instance.

- Move the user-facing guides (install, stack-format, deploying-from-ci, static-sites, backup) from `docs/` into an aphid site at `site/` (single source of truth); `docs/` keeps ADRs, architecture, spikes, open-questions. Update all references (code messages, tests, ADRs, README, CLAUDE.md, install.sh, unit file).
- Default aphid theme (a design pass shared with the admin UI comes later).
- `release.yml`: build the site with `LHelge/aphid` on every run; on `v*` tags pack it into nginx-unprivileged (examples/static-site), push `ghcr.io/lhelge/hoster-docs` and deploy via the OIDC webhook through the `production` environment.
- CI builds the site so broken `[[links]]` fail PRs.
- Served at `docs.hoster.lhelge.se` (wildcard DNS), amd64.

References: docs/static-sites.md, docs/deploying-from-ci.md, examples/static-site/, ADR-0018, ADR-0047, ADR-0052.