---
id: bny
title: "Runtime: one-shot container stats and podman's storage root"
status: open
priority: P1
created: "2026-11-12T00:07:43.293124806Z"
updated: "2026-11-12T00:07:43.293124806Z"
tags:
  - runtime
  - observability
parent: qmw
---

## Summary

Give `ContainerRuntime` what the resource sampler (ADR-0066) needs from podman: a one-shot stats reading of one container, and the path of podman's storage root.

## What

- A `stats(&self, id: &ContainerId)` method on `ContainerRuntime` (`src/runtime/mod.rs`) returning a small typed reading: cumulative CPU time (nanoseconds) and memory in use (bytes), as ADR-0066 defines the memory figure. It is raw counters only; computing CPU % from deltas is the sampler's job.
- `PodmanRuntime` implements it with bollard's one-shot stats (`stream=false`; check what `one_shot` does in podman's compat API and pick the variant that returns promptly). A container that is gone or not running is a typed outcome the sampler can skip, not a logged error.
- `EngineInfo` gains the storage root path (from the engine's info: compat `DockerRootDir`, or libpod `store.graphRoot` — verify against the bollard version in `Cargo.toml` and real podman).
- `FakeRuntime` (`src/runtime/fake.rs`) can be given stats per container for tests.
- A podman-backed test in the style of the existing ones in `src/runtime/podman.rs` (same gating as they use) reading stats of a running container.

## Acceptance Criteria

- [ ] `ContainerRuntime::stats` exists with podman and fake implementations and unit tests for the mapping from bollard's response
- [ ] `EngineInfo` carries the storage root; the System page is unchanged unless trivial
- [ ] `docs/architecture.md` §1 gains a note on what podman's stats endpoint actually returns, if it differs from Docker's
- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

References: ADR-0002, ADR-0066; `docs/architecture.md` §1 (bollard parts only), §8; `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/runtime/fake.rs`.