---
id: "8w4"
title: "Orchestrator and deploy engine v1: full state machine, degenerate health/swap, no proxy"
status: done
priority: P1
created: "2026-11-07T14:45:17.204394447Z"
updated: "2026-11-07T23:53:10.504696499Z"
tags:
  - orchestrator
depends_on:
  - "3v6"
  - p7a
  - h3a
  - "9z8"
  - "4tj"
  - hqs
  - qq8
  - gqp
  - v3m
parent: ame
attempts: 1
---

## Summary
Introduce the orchestrator (single writer of desired state; actor vs handle per OQ-33) and the deploy engine. **Implement the complete state machine of architecture §5 once**, as a pure `(state, event) -> (state, effects)` function with the full state enum. In M3 three states are degenerate because no proxy exists yet: `HealthChecking` uses only the built-in "running and not restarted for 5 s" check, `Switching` records the per-service current pointers without publishing a RouteTable, and `Draining` completes immediately. M6 (`shg`, `v2h`) adds real probes, the RouteTable swap, the `Replacing` effects and the inflight drain; it must not rewrite the engine.

## Acceptance Criteria
- [ ] Orchestrator receives ApplyStack, Deploy{stack, images, source}, RemoveStack, Rollback, Status (shape per OQ-33) and replies with typed results
- [ ] Idempotency: Deploy whose (digest, config hash) per service equals the running ones → Noop (ADR-0005 as refined by ADR-0029; config hash wiring completed in `ax3`)
- [ ] ApplyStack validates, computes the set of services whose config hash changes, and **always** starts a deployment (`source = apply`) for them (ADR-0049). If none change, the revision becomes active with no deployment. A failed apply-deployment leaves the revision active and the stack flagged
- [ ] Full `DeploymentState` enum (Received, Queued, Validating, Pulling, Starting, HealthChecking, Replacing, Switching, Draining, CleaningUp, Live, LiveWithWarnings, Aborting, Failed, Rejected, Noop, Superseded) with a pure transition function and every transition persisted with a timestamp
- [ ] Generation is a per-stack counter assigned per deployment attempt; each **service** records the generation it is currently at. Services whose digest and config are unchanged are not recreated and keep their generation (stack-format §3.1). The commit point is one transaction that writes the deployment's per-service current pointers (architecture §5.4)
- [ ] ApplyStack resolves third-party `repo:tag` images to digests via the registry client and stores them in the stack revision (ADR-0025); deploys only ever use stored digests
- [ ] Validation re-runs stack validation + collision check and allocates ports
- [ ] Containers named/labelled per architecture §8; network ensured; ADR-0048 ordering: Starting/HealthChecking for start-first services, `Replacing` for stop-first services (stop old, start new, built-in check), one commit, then old containers removed
- [ ] One active deployment per stack (queue/coalescing refined in `25t`)

## Implementation Notes
- Effects are data (`PullImage`, `CreateContainer`, `Probe`, `PublishRoutes`, `Persist`, …) executed by a thin interpreter; in M3 `PublishRoutes` and `Probe` have trivial implementations
- All runtime calls via ContainerRuntime so tests use FakeRuntime

## Edge Cases & Considerations
- Pull failure → Aborting → Failed, old generation untouched; stop-first containers stopped in Replacing are restarted
- Unknown stack → typed error reply
- A service that has never received a digest (CI-built image, no webhook yet) is shown as "awaiting first digest" and the stack deploys the other services

## Testing
- Pure transition-function tests for every edge in the §5 diagram (these carry forward unchanged into M6)
- FakeRuntime scenario tests: happy path, noop, apply-with-config-change redeploys only affected services, pull failure, start failure, stop-first failure restarts the old container
- Gated integration test deploying the stack-format example with public stand-in images
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- docs/architecture.md §5
- ADR-0005, ADR-0006, ADR-0017, ADR-0039, ADR-0048, ADR-0049
- OQ-33