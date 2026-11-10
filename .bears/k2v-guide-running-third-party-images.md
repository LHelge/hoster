---
id: k2v
title: "Guide: running third-party images"
status: done
priority: P2
created: "2026-11-09T17:23:50.088833718Z"
updated: "2026-11-10T19:51:00.238784869Z"
tags:
  - docs
depends_on:
  - kbh
  - rp5
  - "5v3"
  - x97
parent: emu
attempts: 1
---

## Summary
A new aphid wiki page, `site/content/wiki/third-party-images.md`, for hosting images you didn't build (Docker Hub, ghcr, quay, any OCI registry). This is the cheap answer to "compose support" until compose is revisited.

Rescoped 2026-11-10 (owner): written now for what exists after M11 and M12. M13 (runtime options and limits) and M14 (`auto_update`) add their own sections in their tasks (77n, xxb, p9n), so this task no longer waits for them.

## Acceptance Criteria
- [x] **Registries and credentials:** image references (explicit registry, `library/` for Docker Hub official images, what counts as third-party), resolve-at-apply and Re-resolve tags, Docker Hub anonymous rate limits and adding a `docker.io` credential under Settings → Registries.
- [x] **Translating a compose file by hand:** a mapping table from compose keys to stack TOML for the fields that exist today: `image`, `environment` / `env_file` → `env` / `secrets`, `ports` → `routes`, named `volumes`, `depends_on`, `healthcheck` → `health.exec`, `configs` → `files` (and compose `secrets` → `files` with `secret`), `command`/`entrypoint`, `user`, `read_only`, `stop_grace_period`, `restart`. Rows marked "planned" (ADR-0059) for `shm_size` / `tmpfs` / `init` and `deploy.resources.limits`, which M13 fills in. A second table lists what has no equivalent and why: bind mounts, `privileged`, `devices`, `cap_add`, `network_mode`, `build`, raw TCP / UDP ports.
- [x] **A complete worked example** (§3): a compose file for a third-party issue tracker (web + worker from one image, postgres, redis) and its hand translation with routes, `depends_on`, exec health and files, each choice explained. `src/stack/tests.rs` (`third_party_example_validates_without_warnings`) parses it from the page, so it passes `hoster stack check` with no warnings.
- [x] **Pitfalls:** bind `0.0.0.0`; `PORT` injection; the health path, and that an image's own HEALTHCHECK is never used (ADR-0056); rootless UID mapping and volume ownership (`podman unshare`); `update = "stop-first"` for anything holding a volume; updating databases across major versions (dump, new volume, restore).
- [x] **Editing in the UI:** Edit stack file opens the active file, the editor checks while you type, snippets and field reference, the stale-revision warning; the `#:schema` line and `hoster stack check` for local editors.
- [x] Linked from overview.md and stack-format.md with `[[third-party-images]]`; `aphid build` passes.

## References
- ADR-0018, ADR-0019, ADR-0025, ADR-0039, ADR-0056, ADR-0057, ADR-0059
- `site/content/wiki/stack-format.md`, `site/content/wiki/overview.md`, `site/content/wiki/backup.md`