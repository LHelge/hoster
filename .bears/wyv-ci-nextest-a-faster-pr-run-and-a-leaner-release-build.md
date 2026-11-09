---
id: wyv
title: "CI: nextest, a faster PR run and a leaner release build"
status: done
priority: P2
created: "2026-11-09T22:28:54.102668690Z"
updated: "2026-11-09T22:32:06.052838998Z"
tags:
  - ci
attempts: 1
---

## Why

A PR run takes as long as its slowest job, the static musl release build (3.5–5 min with fat
LTO); the rest finish in under 2.5 min. One test, `one_gib_streams_with_bounded_memory`, takes
~50 s of the test step. The release workflow reruns fmt and clippy on a commit CI already
checked on `main`. Decided with the owner on 2026-11-09.

## Changes

- [x] Tests run with cargo-nextest in `ci.yml` and `release.yml` (`.config/nextest.toml`, no
      retries); README says how to run them
- [x] On a PR the static musl build only runs when the build inputs changed (`Cargo.toml`,
      `Cargo.lock`, `rust-toolchain.toml`, `build.rs`, the workflows, the two check scripts);
      always on `main`. The job still reports, since it is a required check
- [x] On a PR the 1 GiB streaming test runs at 256 MiB (same 128 MiB memory bound); `main` and
      the release build keep 1 GiB
- [x] `release.yml` no longer runs fmt and clippy; it keeps the tests (the only aarch64 run)
- [x] A new push to a PR cancels the run in progress

## References

- `.github/workflows/ci.yml`, `.github/workflows/release.yml`, README "Development"
- ADR-0052 (release workflow)