---
id: aax
title: Deploy action waits for the deployment's outcome
type: epic
status: done
priority: P2
created: "2026-11-14T21:53:40.992662718Z"
updated: "2026-11-14T22:25:37.840709788Z"
tags:
  - ci
  - webhook
  - docs
---

User feedback of 2026-11-14: the deploy action succeeds on hoster's 202, so a green CI run means only "accepted". A rollout that fails its health checks and keeps the old containers still looks green in GitHub.

## What
- An opt-in `wait: true` input on `actions/deploy` with a `timeout` (default 10m). The action polls the deployment it started until it is terminal: exit 0 on `Live`/`LiveWithWarnings`; non-zero with hoster's state and recorded reason on `Failed`, `Rejected`, `Superseded` or timeout. It prints the deployment URL and writes the outcome to the job summary either way. Plain bash + curl + jq (polling, not SSE).
- A machine-readable, OIDC-authenticated deployment status endpoint for it, since the UI's status partial and SSE stream sit behind the admin session.
- Default behaviour unchanged.

## Tasks
Decision (OQ-45) first; then the endpoint; then the action input with its docs.

References: OQ-45 (docs/open-questions.md), ADR-0042, ADR-0047, `src/web/webhook.rs`, `src/web/stacks/deployments.rs`, `actions/deploy/` (deploy.sh, stub.py, test.sh), `site/content/wiki/deploying-from-ci.md`.