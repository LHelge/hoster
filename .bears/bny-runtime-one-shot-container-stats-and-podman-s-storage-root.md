---
id: bny
title: "Runtime: one-shot container stats and podman's storage root"
status: done
priority: P1
created: "2026-11-12T00:07:43.293124806Z"
updated: "2026-11-12T00:25:04.553470860Z"
tags:
  - runtime
  - observability
parent: qmw
attempts: 1
---

## Summary

Give `ContainerRuntime` what the resource sampler (ADR-0066) needs from podman: a one-shot stats reading of one container, and the path of podman's storage root.

## What

- A `stats(&self, id: &ContainerId)` method on `ContainerRuntime` (`src/runtime/mod.rs`) returning a small typed reading: cumulative CPU time (nanoseconds) and memory in use (bytes), as ADR-0066 defines the memory figure. It is raw counters only; computing CPU % from deltas is the sampler's job.
- `PodmanRuntime` implements it with bollard's stats, `stream(false)` and `one_shot(true)` (ADR-0066 §2; both answer in 3–5 ms on podman 6.1.4 and neither waits for a second sample). CPU is `cpu_stats.cpu_usage.total_usage`, memory is `memory_stats.usage` as podman reports it (already without inactive page cache, ADR-0066 §3); ignore stats' `limit`.
- A container that is gone or not running is a typed outcome the sampler can skip, not a logged error. Podman answers a **stopped** container with HTTP 200 and an error body (`{"cause":"container is stopped",…,"response":500}`), which bollard's all-optional `ContainerStatsResponse` likely decodes as an empty reading: treat a reading without `cpu_stats.cpu_usage.total_usage` or `memory_stats.usage` as not running. A missing container is a 404.
- `EngineInfo` gains the storage root path from compat `/info` `DockerRootDir` (bollard `SystemInfo::docker_root_dir`); on podman 6.1.4 it equals libpod's `store.graphRoot` (ADR-0066, Context).
- `FakeRuntime` (`src/runtime/fake.rs`) can be given stats per container for tests.
- A podman-backed test in the style of the existing ones in `src/runtime/podman.rs` (same gating as they use) reading stats of a running container.

## Acceptance Criteria

- [ ] `ContainerRuntime::stats` exists with podman and fake implementations and unit tests for the mapping from bollard's response
- [ ] `EngineInfo` carries the storage root; the System page is unchanged unless trivial
- [ ] `docs/architecture.md` §1 gains a note on what podman's stats endpoint actually returns where it differs from Docker's (ADR-0066's Context lists what was measured: no `memory_stats.stats`, `usage` = `memory.current − inactive_file`, `limit` = `MemTotal` when unset, empty `precpu_stats`, the 200-with-error body for a stopped container); confirm it with the podman-backed test
- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

References: ADR-0002, ADR-0066; `docs/architecture.md` §1 (bollard parts only), §8; `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/runtime/fake.rs`.