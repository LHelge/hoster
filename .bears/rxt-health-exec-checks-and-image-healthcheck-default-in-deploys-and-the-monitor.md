---
id: rxt
title: "Health: exec checks and image HEALTHCHECK default in deploys and the monitor"
status: open
priority: P1
created: "2026-11-09T17:22:55.298379490Z"
updated: "2026-11-09T17:22:55.298379490Z"
tags:
  - health
  - orchestrator
depends_on:
  - "3rr"
  - p6e
parent: qq9
---

## Summary
Make `Check` run exec probes, and choose the image HEALTHCHECK as the default when the stack file declares no `health`. This applies both to the deploy's health-check step and to the continuous health monitor.

## Acceptance Criteria
- [ ] A new `Kind::Exec` in `src/health/mod.rs`, built from `Probe::Exec` or from an `ImageHealthcheck`. It runs through the runtime's exec. Exit code 0 counts as a success towards `healthy_after`; a non-zero exit or a timeout counts as a failure. The output tail is truncated and recorded as a deployment event via `DeploymentLog`, and so is each status change.
- [ ] `Check::for_service` precedence, per the ADR: declared `health` → image HEALTHCHECK (unless `NONE`) → `GET /healthz` for routed services → settle for the rest. The image's healthcheck is looked up once per deployment from the pinned image; cache it in the `Run`. The engine's `check()` (`src/orchestrator/engine.rs`) passes it in.
- [ ] The monitor (`src/orchestrator/monitor.rs`) probes live generations with the same check, including exec. The `Watched` / `Job` types carry the container id the exec needs.
- [ ] The deployment events and the stack overview name which check was used and why, e.g. "image HEALTHCHECK: pg_isready -U postgres".
- [ ] A container exiting during an exec probe aborts immediately, as today.

## Edge Cases
- `CMD-SHELL` without `/bin/sh` in the image: an unhealthy result with a clear message, not a hang.
- An exec probe overlapping with the next interval: never run two at once per container.
- An image whose HEALTHCHECK `Interval` is very long: clamp to the ADR's bounds so a deploy doesn't wait forever.

## Testing
- `src/health/tests.rs`: precedence table, exec success / failure / timeout with the fake runtime.
- `src/orchestrator/monitor_tests.rs`: exec probing of a live generation.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M11 ADR (task 4qn); ADR-0038, ADR-0051
- `site/content/wiki/stack-format.md` §2.4
- `src/health/mod.rs`, `src/health/liveness.rs`, `src/orchestrator/engine.rs` (`probe`, `check`), `src/orchestrator/monitor.rs`