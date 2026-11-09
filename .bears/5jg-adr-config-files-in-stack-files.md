---
id: "5jg"
title: "ADR: config files in stack files"
status: open
priority: P2
created: "2026-11-09T17:21:18.077629595Z"
updated: "2026-11-09T17:21:18.077629595Z"
tags:
  - decision
  - docs
  - adr
parent: xgf
---

## Summary
Write the ADR (next free number) for M12 and send it to review for owner sign-off. The direction is decided: inline or secret-backed files, mounted read-only, no host paths.

## Points the ADR must settle (proposed answers)
1. **Format.** `[services.<svc>.files]` maps an absolute container path to `{ content = "…" }` or `{ secret = "<stack>/<name>" }`, with an optional `mode` (default `0444`; secrets `0440`?) and an optional `owner = "uid[:gid]"` inside the container.
2. **Validation.**
   - Paths must be absolute and normalized: no `..`, not `/`.
   - A path must not equal or sit inside a volume mount path. Duplicates are errors.
   - Cap each file at 256 KiB and the total per stack at 1 MiB.
3. **Mechanism (the main open point; spike it, then choose).**
   - (a) Write the files to a hoster-owned directory per generation (`$XDG_DATA_HOME/hoster/files/<stack>/<svc>/g<n>/…`, 0700) and mount each file read-only. This is a bind mount, but its source is chosen by hoster, never by the stack file. Explain why it still satisfies ADR-0019.
   - (b) podman secrets with a target path, through the libpod API (check bollard / compat-API support).
   - (c) A per-generation named volume populated through a helper container.
   - Recommend (a) unless the spike shows a problem with rootless UID mapping or SELinux relabelling (`:Z` on Fedora).
4. **UID mapping.** In rootless podman, files owned by the hoster user appear as root in the container. A container running as non-root needs `mode` / `owner` to make the file readable. Document this.
5. **Config hash.** The hash covers inline content and secret versions, so a change redeploys only that service. A rollback recreates the old revision's content.
6. **GC.** Remove file material together with the generation's containers (ADR-0041 keeps old revisions in the store, and they can be re-materialized).
7. **Not in scope:** templating or env interpolation inside files, and writable files.

## Acceptance Criteria
- [ ] ADR written with alternatives and consequences. `docs/adr/README.md` row added. `docs/architecture.md` §8 / §9 updated (new podman object or mount convention, security boundary row).
- [ ] Spike notes for the mechanism recorded in the ADR (podman 6.x, rootless, Ubuntu + Fedora/SELinux if available).
- [ ] Task sent to review for owner sign-off.

## References
- ADR-0019, ADR-0029, ADR-0030, ADR-0041, ADR-0049
- `docs/architecture.md` §8, §9
- `src/runtime/podman.rs` (HostConfig / mounts), `src/runtime/mod.rs` (`ContainerSpec`)