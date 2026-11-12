---
id: "5up"
title: "OTLP: the listener containers reach and the injected OTEL_* variables"
status: open
priority: P2
created: "2026-11-12T19:36:39.410069219Z"
updated: "2026-11-12T19:36:39.410069219Z"
tags:
  - observability
  - runtime
  - stack
depends_on:
  - m7x
  - h4q
parent: xv9
---

## Summary
Serve the receiver where containers can reach it, as spike `h4q` and the ADR from `yjp` decide (OQ-42 answer 2: a hoster listener reached as `host.containers.internal` if that works without host-wide changes and without exposing other host services; otherwise a public route that hairpins through the proxy), and inject the variables that point every service's SDK at it. This is the task most likely to change with the ADR; rewrite it from the ADR before starting.

## Acceptance Criteria
- [ ] The receiver router is served on the path the ADR picks, with the bind address and port it names; nothing else is served there
- [ ] Every managed service gets `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`, `OTEL_EXPORTER_OTLP_HEADERS=Authorization=Bearer <token>`, `OTEL_SERVICE_NAME=<service>` and `OTEL_RESOURCE_ATTRIBUTES` with stack and generation (answer 3A), unless the stack file sets them itself
- [ ] The injection handles the one-time redeploy of every stack as the ADR says (for example by keeping hoster's injected variables out of the config hash); a test shows that upgrading hoster does not redeploy every stack, or the release notes say it does
- [ ] The token appears nowhere in the UI's environment view except masked like a secret (ADR-0029)
- [ ] `hoster doctor` checks that the listener is up and, where it can, that a container on a stack network reaches it
- [ ] `site/content/wiki/stack-format.md` lists the injected variables beside `PORT`

## Implementation Notes
- `PORT` injection (ADR-0051) in `src/orchestrator/plan.rs` and `engine.rs` is the pattern
- If the hairpin route is chosen: it is a reserved host like the admin UI's (ADR-0031), and the proxy's per-address caps (ADR-0065) must not count the host's own exports against a client

## Testing
- Plan/engine tests for the injected variables and for a stack file overriding them; config-hash test; doctor test; an e2e test from a container if the e2e gate allows
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- OQ-42 (questions 2, 3), the ADR from `yjp`, `docs/spikes/` (result of `h4q`), ADR-0006, ADR-0007, ADR-0029, ADR-0031, ADR-0046, ADR-0051, ADR-0065
- `src/orchestrator/plan.rs`, `src/orchestrator/engine.rs`, `src/runtime/podman.rs`, `src/doctor/`