---
id: ax3
title: Env, secrets and named volumes in deployed containers
status: open
priority: P2
created: "2026-11-07T14:45:17.207836167Z"
updated: "2026-11-07T21:50:54.311395196Z"
tags:
  - orchestrator
depends_on:
  - "8w4"
  - "6cm"
parent: ame
---

## Summary
Implement env injection (including the `PORT` convenience, ADR-0051 #4), the encrypted secret store (ADR-0029) and named volumes (ADR-0030).

## Acceptance Criteria
- [ ] `env` injected verbatim; `secrets` resolved from the secret store at create time and never logged
- [ ] `PORT=<container port>` added for a routed service with exactly one distinct container port across its routes, unless `env` defines `PORT`; nothing added for multi-port or non-routed services. The injected value is part of the config hash
- [ ] Secret store: XChaCha20-Poly1305 per-secret nonce, AAD = secret name, stored in SQLite; key from `~/.config/hoster/secret.key` (0600, generated on first start) or a user-scoped systemd credential; set/replace/delete/list-names API (values never returned)
- [ ] Named volumes `hoster-<stack>-<vol>` created on demand, labelled, kept on stack removal
- [ ] Per-service config hash (env incl. injected PORT, referenced secret versions, command, volumes, update strategy, health, routes) computed and stored; deploy idempotency uses (digest, config hash)
- [ ] Validation warns on volumes + start-first (ADR-0039)

## Edge Cases & Considerations
- Missing secret at deploy → Rejected with the secret name (not value)
- Two routes on different container ports → no `PORT`; documented

## Testing
- Unit tests; gated integration test that a volume survives redeploy, secrets appear in container env, and `PORT` is present/absent as specified
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0029, ADR-0030, ADR-0051
- ADR-0019