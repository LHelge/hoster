---
id: j6z
title: "OTLP: per-service tokens and the identity they carry"
status: open
priority: P2
created: "2026-11-12T19:36:02.174497613Z"
updated: "2026-11-12T19:36:02.174497613Z"
tags:
  - observability
  - secrets
  - stack
depends_on:
  - yjp
parent: xv9
---

## Summary
Every managed service gets a bearer token that the receiver maps to its stack and service (OQ-42 question 3, answer A: injected into every service). hoster takes the identity from the token and ignores what the payload claims. Drafted on 2026-11-12 from the owner's answers; check against the ADR from `yjp`.

## Acceptance Criteria
- [ ] A token per (stack, service): random, at least 128 bits, made when the service first deploys, kept encrypted in the secret store (ADR-0029) or stored only as a hash (whichever the ADR says), removed with the stack
- [ ] A lookup from token to (stack, service) for the receiver, constant-time compare, cached in memory and invalidated on change
- [ ] The token does not change on redeploys, so a running old generation and a new one can both export during a start-first deploy
- [ ] The token, its endpoint and the other variables are injected the way the ADR says without changing every stack's config hash (OQ-42 answer 3 asks for this; see how `PORT` of ADR-0051 is treated in `src/orchestrator/plan.rs` and `engine.rs`). The injection itself is in the listener task; this task provides the values
- [ ] Rotation: a "rotate OTLP token" action is out of scope unless the ADR asks for it; note it in the epic if not

## Testing
- Store tests for create, lookup, removal with the stack; a test that a redeploy keeps the token
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- OQ-42 (question 3), the ADR from `yjp`, ADR-0029, ADR-0049, ADR-0051
- `src/secrets/`, `src/store/secrets.rs`, `src/orchestrator/plan.rs`, `src/orchestrator/engine.rs`