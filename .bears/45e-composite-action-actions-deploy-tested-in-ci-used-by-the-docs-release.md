---
id: "45e"
title: Composite action actions/deploy, tested in CI, used by the docs release
status: done
priority: P2
created: "2026-11-13T10:04:37.002430Z"
updated: "2026-11-13T10:08:22.966014Z"
tags:
  - ci
  - webhook
parent: "8kz"
---

## Summary
A composite action at `actions/deploy/` that replaces the curl/jq "Deploy to hoster" step.

## Acceptance Criteria
- [ ] `actions/deploy/action.yml` with inputs `url` (hoster's admin URL; a trailing `/` or `/hooks/deploy` is tolerated), `audience` (default: `url`), `stack`, `images` (`service=digest` lines), `tag` (default: the tag name on a tag push, else the short commit SHA); outputs `result`, `deployment-id`, `deployment-url`
- [ ] The logic is a script next to it (`deploy.sh`), fed only through environment variables (no `${{ }}` in the script body)
- [ ] 200/202 pass; any other status fails with an `::error::` carrying hoster's reason; 502, 503 and transport failures retry (3 attempts) with a fresh single-use token
- [ ] The step summary links the deployment
- [ ] A test script runs the action's script against a local stub of the token endpoint and `/hooks/deploy` (accepted, noop, refused, retry), in a CI job together with shellcheck
- [ ] The docs job in `.github/workflows/release.yml` deploys with `uses: ./actions/deploy`, the tagged copy of the action

## Edge Cases & Considerations
- The token is single-use: a retry must request a new one. A retry after a transport failure is safe because a deploy of the same digests is a no-op (ADR-0029)
- `HOSTER_*` env names are hoster config overrides; the action's own variables must not use that prefix where hoster runs in the same job

## References
- ADR-0042, ADR-0047, ADR-0029, `site/content/wiki/deploying-from-ci.md` §2.3–2.5, `.github/workflows/release.yml` (docs job)