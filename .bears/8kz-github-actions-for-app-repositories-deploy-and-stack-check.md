---
id: "8kz"
title: "GitHub Actions for app repositories: deploy and stack check"
type: epic
status: open
priority: P2
created: "2026-11-13T10:04:28.415257Z"
updated: "2026-11-13T10:04:28.415257Z"
tags:
  - ci
  - docs
  - webhook
---

Owner request of 2026-11-13: the "Deploy to hoster" step that every app repository copies (examples/github-actions/release.yml, examples/static-site/release.yml, the docs job in .github/workflows/release.yml) is ~30 lines of curl and jq, and the stack-check job in the CI guide (§2.6) downloads and checksums a release binary by hand. Replace both with composite actions in this repository, like `LHelge/aphid`'s, and use them ourselves.

## What

- `LHelge/hoster/actions/deploy@vX.Y.Z`: requests the OIDC token for hoster's audience, posts `{stack, images, tag}` to `/hooks/deploy`, fails with hoster's reason, retries 502/503 and transport failures with a fresh token, writes the deployment link to the step summary and exposes `result`, `deployment-id` and `deployment-url`. Pure bash + curl + jq; no binary.
- `LHelge/hoster/actions/stack-check@vX.Y.Z`: downloads the static release binary for the runner's architecture, verifies it against `SHA256SUMS`, runs `hoster stack check`, and turns the diagnostics into PR annotations.

Versioned by hoster's own release tags (`release.sh` already pushes `v*`): the action ref names the server release whose webhook contract (ADR-0042, ADR-0047) it speaks. Subdirectories rather than a root `action.yml`, since there are two actions (no Marketplace listing).

The `hoster` binary is deliberately not used for the deploy: the webhook is one POST, and a 25 MB download buys nothing until hoster can report a deployment's outcome to CI, which needs an authenticated status endpoint and is an architecture question of its own.

## Tasks

`deploy action` first, then `stack-check action`, then the docs task, which needs both.

References: ADR-0042, ADR-0047, ADR-0052, `site/content/wiki/deploying-from-ci.md`, `examples/`, `.github/workflows/release.yml` (docs job), `.github/workflows/ci.yml`.