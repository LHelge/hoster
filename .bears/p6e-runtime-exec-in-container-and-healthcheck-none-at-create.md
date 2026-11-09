---
id: p6e
title: "Runtime: exec in container and Healthcheck NONE at create"
status: done
priority: P1
created: "2026-11-09T17:22:16.197557929Z"
updated: "2026-11-09T20:33:46.624686449Z"
tags:
  - runtime
depends_on:
  - "4qn"
parent: qq9
attempts: 1
---

## Summary
Add the two runtime capabilities the exec checks need: run a command in a container with a timeout, and stop podman from running image HEALTHCHECKs on its own timers.

## Acceptance Criteria
- [ ] A `Runtime` trait method, e.g. `exec(&ContainerId, &[String], Duration) -> Result<ExecOutcome>`. `ExecOutcome` holds the exit code and a bounded tail of stdout+stderr (e.g. 4 KiB). The implementation goes through bollard's exec API (create / start / inspect) on the podman socket, with `AttachStdin: false` and no TTY.
- [ ] A timeout yields a distinct outcome, and the exec process is not left running (best effort: kill via the exec's PID if podman exposes it, otherwise document it).
- [ ] Every container is created with `Healthcheck {Test: ["NONE"]}` (`src/runtime/podman.rs`), so podman never runs a docker-format image's HEALTHCHECK on systemd timers; ADR-0038 owns health.
- [ ] Finished exec sessions are removed per ADR-0056 (libpod exec remove where available, with the documented fallback).
- [ ] The fake runtime (`src/runtime/fake.rs`) supports scripted exec outcomes for the orchestrator tests.
- [ ] Verified against live podman (record the version in `docs/architecture.md` §1 "Verified against"): exec on a rootless container (exit 0, non-zero, cannot-run, timeout), session removal, and that `Healthcheck NONE` stops podman's timer for a docker-format image that declares one.

## Edge Cases
- The container exits while exec is running; the exec binary is missing (non-zero exit / 126 / 127 surfaced as unhealthy with the message); podman's compat-API differences from Docker for exec inspect.

## Testing
- Unit tests for the `HealthConfig` mapping; podman-gated integration test (follow the existing pattern for live podman tests).
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M11 ADR (task 4qn); ADR-0002
- `docs/architecture.md` §1 (bollard notes), §8
- `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/runtime/fake.rs`