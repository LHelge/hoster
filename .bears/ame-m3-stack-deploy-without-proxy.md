---
id: ame
title: "M3: Stack deploy without proxy"
type: epic
status: open
priority: P1
created: "2026-11-07T14:45:17.136663850Z"
updated: "2026-11-07T14:45:17.136663850Z"
---

## Scope

Parse/validate stack files, deploy stacks (recreate strategy) with idempotency by digest, reconcile on startup, and a minimal authenticated web UI (Axum + Askama + HTMX 4, GitHub OAuth) to apply and deploy stacks. ADR-0005, ADR-0006, ADR-0012, ADR-0021.

## Acceptance Criteria

- [ ] Example stack from docs/stack-format.md can be applied and deployed by digest; containers run on hoster-<stack> network
- [ ] Re-deploying the same digests is a no-op
- [ ] Only the configured GitHub admin can sign in