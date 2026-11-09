---
id: xxb
title: "Resource limits: `memory`, `pids`, `cpus` with cgroup pre-flight"
status: open
priority: P2
created: "2026-11-09T17:22:55.315748559Z"
updated: "2026-11-09T17:22:55.315748559Z"
tags:
  - stack
  - runtime
  - ops
depends_on:
  - "77n"
parent: "9q5"
---

## Summary
Per-service limits from the M13 ADR, end to end, plus the cgroup delegation checks that make them trustworthy.

## Acceptance Criteria
- [ ] `Service.limits: Limits { memory: Option<ByteSize>, pids: Option<u32>, cpus: Option<CpuQuota> }`. Validation covers minimums (e.g. memory ≥ 6 MiB), cpus > 0 and ≤ host CPU count (checked as a warning at apply, since the parser is pure), and pids ≥ 1.
- [ ] `ContainerSpec` limits are mapped to `HostConfig.memory`, `memory_swap` (equal to memory, per the ADR), `nano_cpus` and `pids_limit`. They are part of the config hash.
- [ ] A `CgroupControllers` type reads the delegated controllers of hoster's user service (`/sys/fs/cgroup/…/user@<uid>.service/cgroup.controllers`). It is used by `hoster doctor` (one line per controller with the fix) and by apply pre-flight: a stack using a limit whose controller isn't delegated is rejected with the drop-in to add.
- [ ] If the ADR says so, install.sh adds an idempotent `Delegate=` drop-in for `user@.service`, and install.md documents it.
- [ ] The apply preview shows an info note when start-first services with memory limits are redeployed: peak ≈ 2× limit.
- [ ] stack-format.md §2.2 documents limits and the caveats. The schema is regenerated if zcu has landed.
- [ ] Verified on live podman: an OOM-killed container under a small `memory`, pids enforcement, `cpus` throttling visible in `cpu.max`.

## Testing
- Parsing / validation snapshots; HostConfig mapping tests; `CgroupControllers` tests against fixture files; doctor output test.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M13 ADR (task w2w); ADR-0023, ADR-0038
- `site/content/wiki/install.md` (doctor, cgroup delegation), `site/content/wiki/stack-format.md` §2.2
- `src/stack/`, `src/runtime/podman.rs`, `src/doctor/mod.rs`, `src/web/stacks/apply.rs` (pre-flight), install.sh