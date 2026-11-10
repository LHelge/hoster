---
id: w2w
title: "ADR-0059: runtime options and resource limits"
status: done
priority: P2
created: "2026-11-09T17:21:18.085686609Z"
updated: "2026-11-10T00:00:49.305121836Z"
tags:
  - decision
  - docs
  - adr
parent: "9q5"
---

## Summary
Write the ADR (next free number) for M13, amending ADR-0038's "no resource limits in v1", and send it to review for owner sign-off.

## Points the ADR must settle (proposed answers)
1. **Allowlist.**
   - `shm_size = "256MiB"`.
   - `tmpfs = { "/tmp" = "64MiB", "/run" = "8MiB" }`: path → size, mounted `rw,noexec,nosuid,nodev`. Should `noexec` be configurable? Recommend no.
   - `init = true`.
   - Explicitly excluded, and listed in the ADR: `stop_signal` (until needed), `cap_add`, `sysctls`, `devices`, `privileged`, `ulimits`, `security_opt`, `ipc`/`pid` modes.
2. **Limits.**
   - `memory = "512MiB"` is a hard limit. Swap: recommend memory+swap equal to memory, i.e. no swap.
   - `pids = 512`.
   - `cpus = 1.5` maps to NanoCpus.
   - No defaults: unset means unlimited, as today.
   - Should hoster reserve headroom for itself? Recommend documenting it rather than enforcing.
3. **Byte sizes.** One `ByteSize` type: binary suffixes `KiB` / `MiB` / `GiB` plus decimal `KB` / `MB` / `GB`, with `FromStr` / `Display`. It's used by every size field.
4. **Cgroup delegation.**
   - `memory` and `pids` are usually delegated to `user@.service`; `cpu` may not be.
   - Doctor reports which controllers are delegated. Apply pre-flight rejects a stack that uses a limit whose controller is missing, naming the fix (a `Delegate=` drop-in).
   - Should install.sh add the drop-in? Recommend yes, idempotently.
5. **Deploy interaction.** Start-first runs old + new at once, so peak usage is about 2× the limit. Validation gives an info-level note, not a warning.
6. **OOM.**
   - The reconciler reads `State.OOMKilled` when a container exited. It records the reason with the restart, and the monitor / UI show "OOM-killed (limit X)".
   - During a deploy's health check, an OOM exit aborts with that reason.
7. **Config hash.** All options and limits are part of it.

## Acceptance Criteria
- [x] ADR written with alternatives (defaults for everyone, cgroup-v1 support, a wider allowlist) and consequences. ADR-0038's status line updated. Index row added.
- [x] `docs/architecture.md` §8 updated with the new HostConfig fields.
- [x] Task sent to review for owner sign-off.

## References
- ADR-0019, ADR-0023, ADR-0028, ADR-0038
- `docs/architecture.md` §8
- `site/content/wiki/install.md` (cgroup delegation, doctor)
- `src/runtime/podman.rs` (HostConfig), `src/doctor/mod.rs`