---
id: rp5
title: Materialize and mount config files per generation
status: open
priority: P2
created: "2026-11-09T17:22:55.307159730Z"
updated: "2026-11-09T17:22:55.307159730Z"
tags:
  - runtime
  - orchestrator
depends_on:
  - nnq
parent: xgf
---

## Summary
Implement the mount mechanism the M12 ADR chose. Write each generation's files before container create, mount them read-only, and garbage-collect them with the generation.

## Acceptance Criteria
- [ ] A small deep type, e.g. `ConfigFiles` in the runtime or orchestrator, owns materialization. It writes atomically (temp + rename) into the ADR's location with `0700` directories and the declared `mode`, and resolves secret-backed files from the secret store at that moment. It returns the mounts to put on the `ContainerSpec`. Nothing else knows the directory layout.
- [ ] `ContainerSpec` gets a typed file mount (not a free-form bind) that `podman.rs` maps to a read-only mount, with SELinux relabelling if the ADR requires it. A free-form host path stays impossible to express (ADR-0019).
- [ ] The engine (`create_and_start` in `src/orchestrator/engine.rs`) materializes before create. An abort removes that generation's material.
- [ ] GC (`src/orchestrator/gc.rs`) and the reconciler remove material of retired generations. Crash recovery (architecture §5.4) handles material left behind by an interrupted deploy.
- [ ] Rollback to an earlier revision recreates that revision's file content.
- [ ] Verified on live rootless podman, as root and as a non-root `user`: readable at the path, read-only, and owner / mode as documented.

## Edge Cases
- A secret deleted between apply and deploy → the deployment is rejected by name. Disk full while writing → abort cleanly. Container path in a read-only rootfs (`read_only = true`) → still works, since the mount sits on top of the rootfs.

## Testing
- Unit tests for layout, atomic write and GC with a temp dir; orchestrator tests with the fake runtime for abort, rollback and crash recovery; a podman-gated integration test.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M12 ADR (task 5jg); ADR-0019, ADR-0029, ADR-0041
- `docs/architecture.md` §5.4, §8, §9
- `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/orchestrator/engine.rs`, `src/orchestrator/gc.rs`, `src/orchestrator/reconcile.rs`, `src/secrets/`