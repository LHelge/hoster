---
id: rxt
title: "Health: exec checks in deploys and the monitor"
status: open
priority: P1
created: "2026-11-09T17:22:55.298379490Z"
updated: "2026-11-09T18:50:08.895046519Z"
tags:
  - health
  - orchestrator
depends_on:
  - "3rr"
  - p6e
parent: qq9
---

## Summary
Make `Check` run exec probes. This applies both to the deploy's health-check step and to the continuous health monitor.

## Acceptance Criteria
- [ ] A new `Kind::Exec` in `src/health/mod.rs`, built from `Probe::Exec`. It runs through the runtime's exec. Exit code 0 counts as a success towards `healthy_after`; a non-zero exit or a timeout counts as a failure. The output tail is truncated and recorded as a deployment event via `DeploymentLog`, and so is each status change.
- [ ] `Check::for_service` follows ADR-0056: declared `health` (now including `exec`, on any service) → `GET /healthz` for routed services → settle for the rest. On a routed service an exec check also needs the TCP connect to the first route's loopback port, as the ADR says. The engine's `check()` (`src/orchestrator/engine.rs`) passes the container id the exec needs.
- [ ] The monitor (`src/orchestrator/monitor.rs`) probes live generations with the same check, including exec. The `Watched` / `Job` types carry the container id the exec needs.
- [ ] The deployment events and the stack overview name which check was used and why, e.g. "exec: pg_isready -U postgres".
- [ ] A container exiting during an exec probe aborts immediately, as today.

## Edge Cases
- argv[0] missing from the image ("cannot run"): the deploy aborts at once with podman's error; the monitor reports it as `Unhealthy`.
- An exec probe overlapping with the next interval: never run two at once per container.
- A hung exec the hoster user cannot signal (PrivateUsers=yes): it's abandoned per the ADR's cap, never blocking forever.

## Testing
- `src/health/tests.rs`: precedence table, exec success / failure / timeout with the fake runtime.
- `src/orchestrator/monitor_tests.rs`: exec probing of a live generation.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0056 (task 4qn); ADR-0038, ADR-0051
- `site/content/wiki/stack-format.md` §2.4
- `src/health/mod.rs`, `src/health/liveness.rs`, `src/orchestrator/engine.rs` (`probe`, `check`), `src/orchestrator/monitor.rs`