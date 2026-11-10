---
id: kyn
title: "Edge hardening: downstream limits and adversarial HTTP tests"
type: epic
status: open
priority: P2
created: "2026-11-10T12:55:41.446324Z"
updated: "2026-11-10T12:55:41.446324Z"
tags:
  - proxy
  - security
  - test
---

## Goal

hoster embeds Pingora as the internet-facing proxy (ADR-0008), so no Traefik or Caddy sits in
front with defaults shaped by years of exposure. The edge policy is ours. This epic checks what
the embedded proxy does with hostile clients and pins that behaviour with tests, so a Pingora
upgrade cannot change it unnoticed.

The realistic bug classes are logic errors, not memory corruption (Rust, rustls, ring): request
smuggling, routing confusion and denial of service. A remotely triggered panic or resource
blow-up in the proxy also stops the orchestrator and the admin UI, because they share a process.

## Out of scope

- The file-system sandbox for the unit: task `wfw` (OQ-36) already tracks that decision.
- Advisory tracking in CI: its own task outside this epic.
- Splitting the proxy into its own process. That would be a new ADR superseding ADR-0008.

## References

- `docs/adr/0008-embedded-pingora-proxy.md`, `docs/adr/0032-proxy-edge-behaviour.md`,
  `docs/adr/0033-protocol-support-v1.md`, `docs/adr/0011-streaming-bodies-and-per-route-timeouts.md`
- `docs/architecture.md` §9 (security boundaries), §6 (routing)
- `src/proxy/`