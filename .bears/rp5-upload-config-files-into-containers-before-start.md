---
id: rp5
title: Upload config files into containers before start
status: done
priority: P2
created: "2026-11-09T17:22:55.307159730Z"
updated: "2026-11-10T16:09:02.441684547Z"
tags:
  - runtime
  - orchestrator
depends_on:
  - nnq
  - m9b
  - "2eq"
parent: xgf
attempts: 1
---

## Summary
Implement ADR-0057 §3–§5 and the create-time part of §6 in the runtime and the engine: upload each service's `files` into its container between create and start, reveal file secrets there and redact them from exec output. Files live in the container's layer, so there is no host directory, mount or GC of its own. The secret-reference accessor is m9b's and the podman 5.4 gate is 2eq's; both land first.

## Acceptance Criteria
- [x] `ContainerRuntime::image_volumes(&PinnedImage) -> Vec<String>` returns the image's `Config.Volumes`; the fake models it. In Pulling (architecture §5.1), a file equal to, under or an ancestor of an image volume fails the deploy.
- [x] `ContainerSpec` gains `files` (resolved content, mode and owner; no host path, `binds` stays `None`). `create_container` in `podman.rs` uploads them before it returns, as architecture §8 lays out: per path, `HEAD …/archive?path=` on the deepest existing ancestor resolves it through `linkTarget`; the volume, image-volume and reserved-path checks (ADR-0057 §2) run again on the resolved path, and the upload goes there. A path that is itself a symlink, dangling or not, fails naming the link.
- [x] Two tars, both sent with `noOverwriteDirNonDir=true`: the first holds `0:0 0755` entries for every missing parent of any file plus the files with an explicit or `0:0` owner; the second holds the container-user files and is sent with `copyUIDGID=true`. Defaults: `content` is `0:0 0444`, `secret` is `0400` owned by the container's user. Tar header ids are container ids.
- [x] On any upload failure the container is removed and the deploy fails with the text after podman's last `copier: put:` (else a generic message), never host paths or file content.
- [x] The engine reveals file secrets at create, through m9b's accessor, into `ContainerSpec.files`; their content never appears in `Debug` output or logs, like `ContainerSpec.secrets`. A file secret over 256 KiB fails the deploy; file secrets do not count towards the 1 MiB stack cap.
- [x] Exec-output redaction covers file secrets: the whole value and each trimmed line of it (4+ characters).
- [x] Verified on live podman (rootless 6.x; rootful 5.4 if available), as root and as a non-root `user`, with and without `read_only = true`: content at the path, owner / mode as documented, not writable by the non-root user, read-only with `read_only`, files survive a restart and go with the container.

## Result
- Verified on rootless podman 6.1.4 (crun 1.30) by the two live tests in `src/runtime/podman.rs`. No rootful 5.4 was available; dgq adds rootless 5.7 in CI.
- `/etc/mtab` does not exist in a created container (podman links it at start), so a file there uploads. It is not on ADR-0057's reserved list; left as it is.

## Implementation Notes
- Restarts and reboots (ADR-0028) keep the layer; container removal by GC, abort and the reconciler is the only cleanup, so `gc.rs` and crash recovery need no change.
- A rollback recreates inline content from the revision; secret files get the current value (ADR-0041 Correction).
- Keep the upload inside `podman.rs` behind `create_container`; the engine only fills `ContainerSpec.files`.
- No tar crate is in `Cargo.lock` today: add one with `cargo add`. bollard 0.21's `upload_to_container` options have `no_overwrite_dir_non_dir` and `copy_uidgid`.
- The live test is gated by `TEST_HOSTER_PODMAN=1` like the others and skips with a message on a podman older than 5.4. Task dgq makes CI run it on `ubuntu-26.04`.

## Edge Cases
- Secret deleted between apply and deploy → rejected by name and path (m9b's check; the reveal at create repeats it). Owner outside the subuid/subgid range → podman's `error setting ownership` is reduced to the `copier: put:` tail and the partial container is removed.
- File over an image directory → `file exists` (no replace). Path under a symlinked ancestor into a volume (`/var/run/…` with a volume at `/run`) → caught by the re-check on the resolved path.
- Path under `/run`, `/tmp` or `/var/tmp` with `read_only = true` → works (tmpfs refilled from the layer at start), but writable by root.

## Testing
- Fake-runtime tests: image-volume rejection in Pulling, upload failure removes the container, redaction of file-secret lines, an oversized file secret fails the deploy.
- Unit tests for tar layout (two tars, parent entries, owners, modes) and error extraction from a captured podman response.
- A podman-gated integration test for the live checks above.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0057 (§3–§6, §8); ADR-0019, ADR-0028, ADR-0029, ADR-0041
- `docs/architecture.md` §1.7 (archive upload spike), §5.1 (Pulling), §5.5 (output redaction), §8 (Config files bullet), §9 (secrets at rest)
- `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/runtime/fake.rs`, `src/orchestrator/engine.rs`, `src/health/mod.rs`