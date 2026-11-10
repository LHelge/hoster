---
id: rp5
title: Upload config files into containers before start
status: open
priority: P2
created: "2026-11-09T17:22:55.307159730Z"
updated: "2026-11-09T23:58:38.823017608Z"
tags:
  - runtime
  - orchestrator
depends_on:
  - nnq
parent: xgf
---

## Summary
Implement ADR-0057 §3–§9 in the runtime and the engine: upload each service's `files` into its container between create and start, wire secret files through one secret-reference accessor, and require podman 5.4 for stacks with `files`. Files live in the container's layer, so there is no host directory, mount or GC of its own.

## Acceptance Criteria
- [ ] `ContainerRuntime::image_volumes(&PinnedImage) -> Vec<String>` returns the image's `Config.Volumes`; the fake models it. In Pulling (architecture §5.1), a file equal to, under or an ancestor of an image volume fails the deploy.
- [ ] `ContainerSpec` gains `files` (resolved content, mode and owner; no host path, `binds` stays `None`). `create_container` in `podman.rs` uploads them before it returns, as architecture §8 lays out: per path, `HEAD …/archive?path=` on the deepest existing ancestor resolves it through `linkTarget`; the volume, image-volume and reserved-path checks (ADR-0057 §2) run again on the resolved path, and the upload goes there. A path that is itself a symlink, dangling or not, fails naming the link.
- [ ] Two tars, both sent with `noOverwriteDirNonDir=true`: the first holds `0:0 0755` entries for every missing parent of any file plus the files with an explicit or `0:0` owner; the second holds the container-user files and is sent with `copyUIDGID=true`. Defaults: `content` is `0:0 0444`, `secret` is `0400` owned by the container's user. Tar header ids are container ids.
- [ ] On any upload failure the container is removed and the deploy fails with the text after podman's last `copier: put:` (else a generic message), never host paths or file content.
- [ ] One `Service` accessor yields the secret references of env and files. Version collection, ADR-0049's redeploy on a secret change, the missing-secret check (`check_secrets` in `engine.rs`, rejecting by name and path, never a value), the reveal at create, and exec-output redaction all read from it. Redaction also covers each trimmed line (4+ characters) of a file secret. A file secret over 256 KiB fails the deploy; file secrets do not count towards the 1 MiB stack cap.
- [ ] The config hash's secret-file entries carry the version from the accessor's collection, so changing a file secret redeploys only its service (ADR-0057 §7; the hash shape is nnq's).
- [ ] Podman 5.4 or later: a stack with `files` is rejected at apply and a deployment of one fails in `Validating` on an older podman, naming the running and the required version, using the version `podman.rs` reads at connect. Stacks without `files` are unaffected. `hoster doctor` reports the podman version.
- [ ] Verified on live podman (rootless 6.x; rootful 5.4 if available), as root and as a non-root `user`, with and without `read_only = true`: content at the path, owner / mode as documented, not writable by the non-root user, read-only with `read_only`, files survive a restart and go with the container.

## Implementation Notes
- Restarts and reboots (ADR-0028) keep the layer; container removal by GC, abort and the reconciler is the only cleanup, so `gc.rs` and crash recovery need no change.
- A rollback recreates inline content from the revision; secret files get the current value (ADR-0041 Correction).
- Keep the upload inside `podman.rs` behind `create_container`; the engine only fills `ContainerSpec.files`.

## Edge Cases
- Secret deleted between apply and deploy → rejected by name and path. Owner outside the subuid/subgid range → podman's `error setting ownership` is reduced to the `copier: put:` tail and the partial container is removed.
- File over an image directory → `file exists` (no replace). Path under a symlinked ancestor into a volume (`/var/run/…` with a volume at `/run`) → caught by the re-check on the resolved path.
- Path under `/run`, `/tmp` or `/var/tmp` with `read_only = true` → works (tmpfs refilled from the layer at start), but writable by root.

## Testing
- Fake-runtime tests: image-volume rejection in Pulling, upload failure removes the container, version check in `Validating`, missing file secret, redaction of file-secret lines, file-secret change redeploys only its service.
- Unit tests for tar layout (two tars, parent entries, owners, modes) and error extraction from a captured podman response.
- A podman-gated integration test for the live checks above.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0057 (§3–§9); ADR-0019, ADR-0028, ADR-0029, ADR-0041, ADR-0049
- `docs/architecture.md` §1.7 (archive upload spike), §5.1 (Validating, Pulling), §5.5 (output redaction), §8 (Config files bullet), §9 (secrets at rest)
- `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/runtime/fake.rs`, `src/orchestrator/engine.rs`, `src/orchestrator/plan.rs`, `src/orchestrator/stack_secrets.rs`, `src/health/mod.rs`, `src/doctor/mod.rs`, `src/web/stacks/apply.rs`
