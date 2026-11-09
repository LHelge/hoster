---
id: "6z3"
title: "M14: Third-party image updates"
type: epic
status: open
priority: P2
created: "2026-11-09T17:20:07.827541609Z"
updated: "2026-11-09T17:20:07.827541609Z"
tags:
  - registry
  - orchestrator
  - web
  - third-party
---

## Scope

Today a third-party image's tag (`postgres:16`, `valkey:8`, `immich-server:v2.3.1`) is resolved to a digest at apply and never changes until someone edits the file or presses "Re-resolve tags" (ADR-0025). This epic adds update detection, a one-click update, and an opt-in auto-update policy. Branch: `epic/<id>-third-party-updates`.

Owner decision (2026-11-09): **notify + opt-in auto-update**. By default nothing deploys by itself.

- **Detection:** a background checker, much slower than the ghcr poller, sees two kinds of update. (a) A floating tag such as `16` or `latest` now points at a different digest. (b) A newer version tag exists in the same line, e.g. `v2.3.1` → `v2.3.4` / `v2.4.0`.
- **Notify:** "update available" on the stack overview, the stacks list and the dashboard warnings. A one-click update goes through the normal apply preview and diff.
- **Auto-update:** a per-service opt-in policy, e.g. `auto_update = "patch"`, applies matching updates as a new revision automatically and records it as its own deployment source.

## Acceptance Criteria

- [ ] An ADR records the detection rules, the version-line matching, the policy values and the rate-limit behaviour, amending ADR-0025 / ADR-0043.
- [ ] Available updates are detected for Docker Hub, ghcr and generic OCI registries, with credentials when configured and backoff on 429.
- [ ] The UI shows available updates and applies one (or all for a stack) through the existing apply preview.
- [ ] Services with `auto_update` are updated in one deployment per stack. A failed auto-update is not retried for the same target.
- [ ] Services sharing a version (e.g. app server + worker of the same release) move together.
- [ ] Docs cover the policy and its risks.

## References

- ADR-0025, ADR-0040, ADR-0041, ADR-0043, ADR-0049
- `docs/architecture.md` §5.2 (idempotency and coalescing)
- `site/content/wiki/stack-format.md` §2.2
- Related epic: c5s (notifications)