---
id: hxh
title: "M7: Webhook and digest poller"
type: epic
status: done
priority: P1
created: "2026-11-07T14:45:17.145566088Z"
updated: "2026-11-08T02:45:53.405507913Z"
---

## Scope

Deploy webhook carrying digests, authenticated with GitHub Actions OIDC tokens (ADR-0047), edge-triggered poller fallback (ADR-0043), CI example. ADR-0004, ADR-0005, ADR-0042.

## Acceptance Criteria

- [ ] A GitHub Actions release step (OIDC, no stored secret) deploys a new digest end to end
- [ ] A missed webhook is caught by the poller; duplicates are no-ops