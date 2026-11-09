---
id: "77n"
title: "Runtime options: `shm_size`, `tmpfs`, `init`"
status: open
priority: P2
created: "2026-11-09T17:22:16.215170357Z"
updated: "2026-11-09T17:22:16.215170357Z"
tags:
  - stack
  - runtime
depends_on:
  - w2w
parent: "9q5"
---

## Summary
Add the allowlisted runtime options from the M13 ADR end to end: stack format → model → `ContainerSpec` → bollard `HostConfig`. Introduce the shared `ByteSize` type here.

## Acceptance Criteria
- [ ] A `ByteSize` type (`FromStr`, `Display`, `serde` via `FromStr`) accepting the ADR's suffixes, with clear errors. It lives in `src/stack/` next to the duration handling, and is reused by `memory` in the limits task.
- [ ] `Service.shm_size: Option<ByteSize>`, `Service.tmpfs: BTreeMap<ContainerPath, ByteSize>`, `Service.init: bool`. Validation: tmpfs paths must not collide with volume or file paths. Reuse `ContainerPath` if M12 landed first; otherwise introduce it here, and M12 reuses it.
- [ ] `ContainerSpec` gains the fields (`src/runtime/mod.rs`); `podman.rs` maps them to `HostConfig.shm_size`, `HostConfig.tmpfs` (with `rw,noexec,nosuid,nodev,size=…`) and `HostConfig.init`. Nothing outside the allowlist becomes settable.
- [ ] Config hash covers all three.
- [ ] Verified on live podman: `df /dev/shm` and the tmpfs size inside a container; PID 1 is catatonit with `init = true`.
- [ ] stack-format.md §2.2 documents the fields and explicitly lists what stays unexpressible. The schema is regenerated if zcu has landed.

## Testing
- `ByteSize` round-trip and error tests; stack snapshots; a HostConfig mapping unit test in `podman.rs`; config-hash tests.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M13 ADR (task w2w); ADR-0019
- `docs/architecture.md` §8
- `src/stack/raw.rs`, `src/stack/mod.rs`, `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/orchestrator/plan.rs`