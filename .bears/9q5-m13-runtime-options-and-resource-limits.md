---
id: "9q5"
title: "M13: Runtime options and resource limits"
type: epic
status: open
priority: P2
created: "2026-11-09T17:20:07.818001581Z"
updated: "2026-11-09T17:20:07.818001581Z"
tags:
  - stack
  - runtime
  - third-party
---

## Scope

A small, explicit allowlist of container options that general images need, plus per-service resource limits so one runaway container can't take down the VPS, hoster or the proxy. Branch: `epic/<id>-runtime-options-limits`.

- **Runtime options:** `shm_size` (Postgres, headless browsers), `tmpfs` mounts (writable scratch for `read_only = true`), `init` (catatonit as PID 1). `stop_signal` is deliberately left out until something needs it.
- **Limits:** `memory`, `pids`, and optionally `cpus`. ADR-0038 left these out of v1 and anticipated them once cgroup delegation was checked.
- **OOM visibility:** a container killed for exceeding its memory limit is shown as such, not as an anonymous crash loop.

Still not expressible: `cap_add`, `sysctls`, `devices`, `privileged`, `ulimits`, bind mounts, GPUs (ADR-0019; owner 2026-11-09).

## Acceptance Criteria

- [ ] An ADR records the allowlist and the limit semantics, amending ADR-0038.
- [ ] Each option and limit reaches the container's config and is part of the config hash.
- [ ] `hoster doctor` and apply pre-flight report missing cgroup controllers (e.g. `cpu` not delegated) clearly.
- [ ] OOM kills are recorded and shown on the stack overview, in deployment events and in dashboard warnings.
- [ ] The stack-format reference documents every new field, including the start-first peak (old + new) caveat.

## References

- ADR-0019, ADR-0023, ADR-0028, ADR-0038
- `docs/architecture.md` §8
- `site/content/wiki/stack-format.md` §2.2, `site/content/wiki/install.md` (doctor, cgroup delegation)
- Related epic: qmw (resource usage meters)