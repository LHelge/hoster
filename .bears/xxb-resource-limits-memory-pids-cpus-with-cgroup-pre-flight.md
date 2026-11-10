---
id: xxb
title: "Resource limits: `memory`, `pids`, `cpus` with cgroup pre-flight"
status: open
priority: P2
created: "2026-11-09T17:22:55.315748559Z"
updated: "2026-11-10T00:00:49.305121836Z"
tags:
  - stack
  - runtime
  - ops
depends_on:
  - "77n"
parent: "9q5"
---

## Summary
Implement ADR-0059 §2, §4 and §5: the per-service `limits` table end to end, the cgroup-controller check that makes limits trustworthy, the doctor hint, and install.sh's delegation drop-in and catatonit.

## Acceptance Criteria
- [ ] **Model.** `[services.<svc>.limits]` (unknown keys rejected) → `Service.limits: Limits { memory: Option<ByteSize>, pids: Option<u32>, cpus: Option<Cpus> }`. No defaults: unset keeps podman's (no memory or CPU cap, 2048 pids).
- [ ] **Errors** (parser, with field paths): `memory` below 8 MiB, `pids` below 1, `cpus` not finite or outside 0.01–1024.
- [ ] **Warning** (parser): `shm_size` plus the tmpfs sizes reach `memory` (≥), since that RAM is charged to the limit.
- [ ] **Create:** `Memory` and `MemorySwap` both get the `memory` bytes (no swap); `NanoCpus` = round(cpus × 10⁹); `PidsLimit` = `pids`. Unset limits stay unset.
- [ ] **Hash:** each limit is added only when set (`memory` as bytes, `cpus` as nano-CPUs, `pids` as is). An empty `limits` table counts as unset, so no existing hash changes.
- [ ] **Controller check:** a `CgroupControllers` type reads `/sys/fs/cgroup/user.slice/user-<uid>.slice/user@<uid>.service/cgroup.controllers`. Doctor and the orchestrator share it.
  - A limit whose controller (`memory`, `pids`, `cpu`) is missing rejects the stack.
  - It runs at apply (`Orchestrator::prepare`, after `check_routes`, before `resolve_images`) and in the engine's `Validating`, so CI deploys and rollbacks are covered.
  - An unreadable file rejects only stacks that set a limit.
  - The error names the missing controllers and the fix: `/etc/systemd/system/user@<uid>.service.d/hoster-delegate.conf` with `[Service]` `Delegate=cpu memory pids`, then a restart of `user@<uid>.service` or a reboot.
- [ ] **Live check:** `Orchestrator::check_file` (the editor's diagnostics while typing, M15 x97) runs the controller check too, as an error at the offending `limits` key (`FieldPath::locate`), and the host warnings as warnings.
- [ ] **Host warnings at apply:** `cpus` above the host's CPU count, or `memory` above `MemTotal`.
- [ ] **Doctor:** the `cgroup_delegation` hint (`src/doctor/mod.rs`) names the same drop-in path and content instead of today's `user@.service.d/delegate.conf` with `cpuset io`. It explains that a reload is not enough, only a restart of the user manager or a reboot.
- [ ] **install.sh** (amends ADR-0023 and ADR-0052):
  - After enabling linger, it writes the drop-in if the user manager lacks one of `cpu memory pids`. It never writes over an existing file, then runs `systemctl daemon-reload`.
  - If `hoster.service` is neither enabled nor active and no `hoster-*` container runs, it restarts `user@<uid>.service` before enabling `podman.socket`. Otherwise it prints that the change applies when that unit restarts (stopping hoster and its containers) or at reboot.
  - It installs `catatonit` where missing, by apt-get or dnf, the same way it installs podman.
- [ ] **Docs.**
  - stack-format.md §2.2 documents `limits`: units, bounds, no swap, no defaults, and that shm, tmpfs, init and exec checks count against the limits. It documents once that start-first runs old and new together, so a service can use twice its limits in the overlap, and advises leaving headroom for hoster.
  - install.md documents delegation, the drop-in, when install.sh restarts the user manager, and catatonit on other distributions (Arch: `/usr/lib/podman/catatonit`).
- [ ] Verified on live podman:
  - a small `memory` OOM-kills the container, and `memory.swap.max` is 0;
  - `pids.max` matches `pids`;
  - `cpu.max` reads `150000 100000` for `cpus = 1.5`;
  - a stack with a limit on a host missing that controller is rejected at apply.
- [ ] The stack JSON Schema covers `limits` (see Implementation Notes).
- [ ] The third-party images guide (`site/content/wiki/third-party-images.md`, task k2v) fills in its `deploy.resources.limits` compose-mapping row and adds `limits` to its worked example, plus the start-first peak memory pitfall.

## Implementation Notes
- `Cpus` is a newtype over `f64` with `FromStr`/`TryFrom`. The rounding to nano-CPUs lives on it (`i64`), so create and the hash share one conversion.
- Reuse the `MemTotal` and CPU-count readers in `src/web/system/host.rs`. Move them to a shared home if needed, rather than parsing `/proc` twice. Warnings travel in `ApplyOutcome.warnings`.
- `Validating` failures go to `Rejected` with reservations released (architecture §5.1). Use the same error text as apply.
- **Schema:** the stack JSON Schema (`src/stack/schema.rs`, `site/static/stack.schema.json` with a drift test) and `hoster stack check` are on main. Add `limits` to the raw types' schema (with bounds where the schema can express them) and regenerate. If the editor-help task (qjx) has landed, add a `limits` snippet. `hoster stack check` gets the parser errors and the shm/tmpfs warning, but not the host-dependent checks.
- install.sh changes are checked by shellcheck in CI and `scripts/test-install.sh`.

## Edge Cases
- `cpus = 0.001` is an error here, even though podman accepts it and raises it to the kernel minimum.
- `pids = 0` is an error: podman would make it `pids.max` 1, and the container could not fork.
- A controller undelegated after apply makes starts fail (HTTP 500, container left `created`). The reconciler's handling of that is r3r's; doctor names the gap.
- A re-run of install.sh on a host where hoster runs only writes the drop-in (if missing) and prints the notice, never restarting the user manager.

## Testing
- Parsing and validation snapshots (bounds, the warning, the empty table); create-config mapping tests (`MemorySwap` equal to `Memory`, the `NanoCpus` rounding); config-hash tests.
- `CgroupControllers` against fixture files (missing file, partial list). Orchestrator tests: apply and `Validating` rejection, and an unreadable file with and without limits. Doctor output test for the new hint.
- `scripts/test-install.sh` cases for the drop-in: missing controller, existing file, and the guarded restart.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0059 §2, §4, §5; ADR-0023, ADR-0038, ADR-0052
- `docs/architecture.md` §1.8 (Fields, Swap, Odd values, Controllers, Delegation, Init), §5.1 (`Validating`), §8 (Create-time container config), §9
- `site/content/wiki/install.md` (doctor, cgroup delegation), `site/content/wiki/stack-format.md` §2.2
- `src/stack/`, `src/runtime/podman.rs`, `src/orchestrator/mod.rs` (`prepare`), `src/orchestrator/engine.rs` (`validate`), `src/doctor/mod.rs`, `src/web/system/host.rs`, `install.sh`
