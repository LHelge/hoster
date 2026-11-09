---
id: p6e
title: "Runtime: exec in container and image HEALTHCHECK inspection"
status: open
priority: P1
created: "2026-11-09T17:22:16.197557929Z"
updated: "2026-11-09T17:22:16.197557929Z"
tags:
  - runtime
depends_on:
  - "4qn"
parent: qq9
---

## Summary
Add the two runtime capabilities the exec checks need: run a command in a container with a timeout, and read an image's declared HEALTHCHECK.

## Acceptance Criteria
- [ ] A `Runtime` trait method, e.g. `exec(&ContainerId, &[String], Duration) -> Result<ExecOutcome>`. `ExecOutcome` holds the exit code and a bounded tail of stdout+stderr (e.g. 4 KiB). The implementation goes through bollard's exec API (create / start / inspect) on the podman socket, with `AttachStdin: false` and no TTY.
- [ ] A timeout yields a distinct outcome, and the exec process is not left running (best effort: kill via the exec's PID if podman exposes it, otherwise document it).
- [ ] An image inspect method, e.g. `image_healthcheck(&PinnedImage) -> Result<Option<ImageHealthcheck>>`. It maps `Config.Healthcheck`: `Test` (`CMD` → argv, `CMD-SHELL` → `["/bin/sh","-c",s]`, `NONE` → explicitly disabled), plus `Interval` / `Timeout` / `StartPeriod` / `Retries` in nanoseconds. Behaviour lives on the `ImageHealthcheck` type (`TryFrom<bollard::models::HealthConfig>`).
- [ ] The fake runtime (`src/runtime/fake.rs`) supports scripted exec outcomes and image healthchecks for the orchestrator tests.
- [ ] Verified against live podman (record the version in `docs/architecture.md` §1 "Verified against"): exec on a rootless container, image healthcheck read from a real image (e.g. postgres, redis).

## Edge Cases
- The container exits while exec is running; the exec binary is missing (non-zero exit / 126 / 127 surfaced as unhealthy with the message); podman's compat-API differences from Docker for exec inspect.

## Testing
- Unit tests for the `HealthConfig` mapping; podman-gated integration test (follow the existing pattern for live podman tests).
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M11 ADR (task 4qn); ADR-0002
- `docs/architecture.md` §1 (bollard notes), §8
- `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/runtime/fake.rs`