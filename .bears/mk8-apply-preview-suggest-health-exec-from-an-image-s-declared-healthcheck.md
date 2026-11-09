---
id: mk8
title: "Apply preview: suggest `health.exec` from an image's declared HEALTHCHECK"
status: proposed
priority: P3
created: "2026-11-09T18:50:40.802490737Z"
updated: "2026-11-09T18:50:44.155578765Z"
tags:
  - web
  - registry
  - health
depends_on:
  - rxt
parent: emu
---

## Summary
An advisory follow-up to ADR-0056, which dropped the image HEALTHCHECK as a default check. Podman drops HEALTHCHECK from OCI-format images, so reading it needs the image config blob from the registry. Here that read is purely advisory: when a service declares no `health` and its image declares a HEALTHCHECK, the apply preview shows "the image declares `HEALTHCHECK CMD /healthcheck.sh`" with a ready-to-paste `health.exec` snippet. It never changes which check runs.

## Acceptance Criteria
- [ ] The registry client fetches the config blob for a pinned digest (one GET, sha256-verified) and parses `config.Healthcheck`: `CMD` maps to an argv, `CMD-SHELL` to `["/bin/sh","-c",…]` or the config's `Shell`, and `NONE` / empty gives no suggestion.
- [ ] The fetch runs only while building an apply / re-resolve / update preview, with a short timeout. Any failure simply means no suggestion: it never fails or delays an apply.
- [ ] The preview aside shows the suggestion per service, with the snippet and a note that the shell form needs `/bin/sh` in the image.

## References
- ADR-0056 (Alternatives: image HEALTHCHECK default), ADR-0025
- `src/registry/client.rs`, `src/web/stacks/apply.rs`, `templates/apply_preview.html`