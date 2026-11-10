---
id: k2v
title: "Guide: running third-party images"
status: open
priority: P2
created: "2026-11-09T17:23:50.088833718Z"
updated: "2026-11-10T17:52:31.931192851Z"
tags:
  - docs
depends_on:
  - kbh
  - rp5
  - "5v3"
  - x97
parent: emu
---

## Summary
A new aphid wiki page, `site/content/wiki/third-party-images.md`, for hosting images you didn't build (Docker Hub, ghcr, quay, any OCI registry). This is the cheap answer to "compose support" until compose is revisited.

Rescoped 2026-11-10 (owner): written now for what exists after M11 and M12. M13 (runtime options and limits) and M14 (`auto_update`) add their own sections in their tasks (77n, xxb, p9n), so this task no longer waits for them.

## Acceptance Criteria
- [ ] **Registries and credentials:** image references, resolve-at-apply, Docker Hub anonymous rate limits and adding a `docker.io` credential.
- [ ] **Translating a compose file by hand:** a mapping table from compose keys to stack TOML for the fields that exist today: `image`, `environment` / `env_file` → `env` / `secrets`, `ports` → `routes`, named `volumes`, `depends_on`, `healthcheck` → `health.exec`, `configs` → `files`. Leave room (one row each, marked "planned") for `shm_size` / `tmpfs` / `init` and `deploy.resources.limits`, which M13 fills in. Also list what has no equivalent and why: bind mounts, `privileged`, `devices`, `cap_add`, `network_mode`, `build`, raw TCP / UDP ports.
- [ ] **A complete worked example** of a generic multi-service app: web app + postgres + redis + background worker, with routes, `depends_on`, exec health and files. Explain each choice. The example passes `hoster stack check`, which a test or the docs-example schema test covers.
- [ ] **Pitfalls:**
  - bind `0.0.0.0`, not `127.0.0.1`;
  - `PORT` injection;
  - the health path, and that an image's own HEALTHCHECK is never used (translate it to `health.exec`, ADR-0056);
  - rootless UID mapping and volume ownership;
  - `update = "stop-first"` for anything holding a volume;
  - updating databases across major versions.
- [ ] **Editing in the UI:** a short note that the stack page's Edit opens the active file and that the editor checks the file while you type (5v3, x97). Mention the `#:schema` line for local editors.
- [ ] Linked from overview.md and stack-format.md with `[[third-party-images]]`; `aphid build` passes.

## References
- ADR-0018, ADR-0019, ADR-0025, ADR-0039, ADR-0056, ADR-0057
- `site/content/wiki/stack-format.md`, `site/content/wiki/overview.md`, `site/content/wiki/backup.md`