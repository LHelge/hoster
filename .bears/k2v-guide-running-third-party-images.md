---
id: k2v
title: "Guide: running third-party images"
status: open
priority: P2
created: "2026-11-09T17:23:50.088833718Z"
updated: "2026-11-09T17:23:50.088833718Z"
tags:
  - docs
depends_on:
  - kbh
  - rp5
  - xxb
  - j6w
parent: emu
---

## Summary
A new aphid wiki page, `site/content/wiki/third-party-images.md`, for hosting images you didn't build (Docker Hub, ghcr, quay, any OCI registry). This is the cheap answer to "compose support" until compose is revisited.

## Acceptance Criteria
- [ ] **Registries and credentials:** image references, resolve-at-apply, Docker Hub anonymous rate limits and adding a `docker.io` credential.
- [ ] **Translating a compose file by hand:** a mapping table from compose keys to stack TOML (`image`, `environment` / `env_file` → `env` / `secrets`, `ports` → `routes`, named `volumes`, `depends_on`, `healthcheck` → `health.exec`, `configs` → `files`, `shm_size` / `tmpfs` / `init`, `deploy.resources.limits` → limits). Also list what has no equivalent and why: bind mounts, `privileged`, `devices`, `cap_add`, `network_mode`, `build`, raw TCP / UDP ports.
- [ ] **A complete worked example** of a generic multi-service app: web app + postgres + redis + background worker, with routes, `depends_on`, exec health, files, limits and `auto_update`. Explain each choice.
- [ ] **Pitfalls:** bind `0.0.0.0` not `127.0.0.1`; `PORT` injection; health path / image HEALTHCHECK; rootless UID mapping and volume ownership; `update = "stop-first"` for anything holding a volume; start-first peak memory; updating databases across major versions.
- [ ] Linked from overview.md and stack-format.md with `[[third-party-images]]`; `aphid build` passes.

## References
- ADR-0018, ADR-0019, ADR-0025, ADR-0039
- `site/content/wiki/stack-format.md`, `site/content/wiki/overview.md`, `site/content/wiki/backup.md`
- Builds on M11–M14; if one of them is descoped, drop its parts here rather than waiting.