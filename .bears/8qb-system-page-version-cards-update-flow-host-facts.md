---
id: "8qb"
title: "System page: version cards, update flow, host facts"
status: done
priority: P2
created: "2026-11-08T18:21:22.668782812Z"
updated: "2026-11-08T23:11:46.630533322Z"
tags:
  - web
  - ui
  - update
depends_on:
  - xhh
parent: "8bf"
attempts: 1
---

Redesign `/system` after `docs/design/System.dc.html`, without the images & storage and update-history parts (follow-ups).

- **Header**: "check for updates" (ghost, the existing click-only check, ADR-0052).
- **Version card**: RUNNING (version in large mono, with target, schema version and hoster's own uptime below) and PREVIOUS (installed previous version). Instead of a roll-back button, a mono hint `ssh … && hoster rollback` with a link to the manual recovery guide. No release channel.
- **Update card**: the check result as the design's "UPDATE AVAILABLE" panel (version, release date, a release-notes excerpt in a scroll box, a full-changelog link), "Apply update" as the primary action with the existing confirm, and a note that stacks keep serving while hoster's own connections drop for a few seconds. The restart page (`system_restarting.html`) becomes the step list (✓ verified checksum, ✓ snapshot, ● restarting …) as far as hoster knows those steps, without inventing any.
- **Host facts** (new backend, small): hostname, OS (`/etc/os-release` PRETTY_NAME), kernel (uname), architecture and CPU count, total memory, container runtime (podman version from the API, rootless), cgroups version, public addresses, data dir, host uptime (`/proc/uptime`). Read when the page is rendered, behind a small `HostFacts` type with a fake for tests. Missing facts show "unknown" and don't fail the page.

References: ADR-0002, ADR-0052, `src/web/system.rs`, `src/update/`, `src/runtime/`, `templates/system.html`, `templates/system_check.html`, `templates/system_restarting.html`, `docs/design/System.dc.html`.