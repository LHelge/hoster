---
id: "9q5"
title: "M13: Runtime options and resource limits"
type: epic
status: done
priority: P2
created: "2026-11-09T17:20:07.818001581Z"
updated: "2026-11-10T21:58:12.149691244Z"
tags:
  - stack
  - runtime
  - third-party
---

## Scope

Implement ADR-0059: a small allowlist of container options that general images need, per-service resource limits so one runaway container cannot starve the VPS, hoster or the proxy, and OOM kills named as such. Branch: `epic/9q5-runtime-options-limits`.

- **Runtime options (77n):** `shm_size`, `tmpfs` (path → size, mounted `rw,nosuid,nodev,mode=1777,size=<bytes>`, exec allowed) and `init` (catatonit as PID 1), with one `ByteSize` type for every size.
- **Limits (xxb):** a `[services.<svc>.limits]` table with `memory` (hard, no swap), `pids` and `cpus`. No defaults. A cgroup-controller check at apply and in `Validating`, doctor's hint, and install.sh's `Delegate=cpu memory pids` drop-in plus catatonit.
- **OOM (r3r):** the sticky `OOMKilled` flag tracked per container, so restart events, restart records (crash-loop detail, warnings) and deploy aborts name an OOM kill; limits shown in the UI.

Stays unexpressible (unknown fields): `stop_signal`, `cap_add`, `cap_drop`, `sysctls`, `devices`, `privileged`, `ulimits`, `security_opt`, `ipc`/`pid` modes, OOM-killer tuning, cpuset and io limits, reservations, CPU shares, bind mounts, GPUs (ADR-0019, ADR-0059 Alternatives).

The stack JSON Schema (`src/stack/schema.rs`, `site/static/stack.schema.json` with a drift test) and `hoster stack check` are on main, so every new field must extend the schema. Once M15 (emu) lands, the new fields also get editor snippets (qjx) and their sections of the third-party images guide (k2v); 77n and xxb carry those criteria. The branch's old `docs(adr): ADR-0058` commit is stale (the ADR landed on main as ADR-0059), so recreate the branch from main.

## Acceptance Criteria

- [x] ADR-0059 records the allowlist, limits, `ByteSize`, hashing, delegation and OOM rules (task w2w).
- [ ] Every option and limit reaches the container's create config exactly as ADR-0059 §4 / architecture §8 say. Each enters the config hash only when set, so no existing hash changes.
- [ ] A stack that sets a limit whose controller the user manager does not delegate is rejected at apply and in `Validating`, naming the one fix (`hoster-delegate.conf`). Doctor's `cgroup_delegation` hint names the same fix.
- [ ] install.sh writes the `Delegate=` drop-in only where a controller is missing, restarts the user manager only on a fresh install, and installs catatonit.
- [ ] OOM kills are named per ADR-0059 §6 in restart events, the restart record (crash-loop detail and dashboard warnings) and deploy aborts. Configured limits and options are shown on the stack overview.
- [ ] The stack-format reference documents every new field once, including the start-first overlap (old and new running together can use twice the limits), the headroom advice and that shm/tmpfs count against `memory`. The install guide documents delegation and catatonit.
- [ ] The stack JSON Schema covers the new fields (after the rebase onto the emu work).

## References

- ADR-0059 (all sections); ADR-0019, ADR-0023, ADR-0028, ADR-0038, ADR-0049, ADR-0052, ADR-0055, ADR-0056
- `docs/architecture.md` §1.8, §5.1 (`Validating`, `HealthChecking`, `Replacing`), §8, §9, §11, §2.1 (Reconciler)
- `site/content/wiki/stack-format.md` §2.2, `site/content/wiki/install.md` (doctor, cgroup delegation)
- Related epics: emu (stack schema, `hoster stack check`), qmw (resource usage meters)
