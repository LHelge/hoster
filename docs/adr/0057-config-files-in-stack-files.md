# ADR-0057: Stack files carry config files, which hoster writes into each container before it starts

- Status: Accepted (amends ADR-0029)
- Date: 2026-11-10

## Context

Many third-party images (Prometheus, Grafana, nginx, Mosquitto) read a config file, not env
vars. Bind mounts are excluded (ADR-0019), so the stack file carries the content, inline or as a
secret (ADR-0029), surviving restarts, reboots (ADR-0028) and rollback (ADR-0041). Spike:
architecture §1.7 (podman 6.1.4 rootless; 5.7.0, 5.4.2 and 4.9.3 rootful; no SELinux).

## Decision

**1. Format.** `[services.<svc>.files]` maps an absolute container path to an inline table with
exactly one of `content = "…"` (a TOML string, so UTF-8 text) or `secret = "<stack>/<name>"` (a
secret of the same stack). `mode` and `owner` are optional:

```toml
[services.prometheus.files]
"/etc/prometheus/prometheus.yml" = { content = """…""" }
"/etc/app/credentials.json"      = { secret = "monitoring/creds", mode = "0440", owner = "0:65534" }
```

`mode` is an octal string from `"0000"` to `"0777"`, so setuid, setgid and sticky bits are
rejected. `owner` is a numeric `"uid:gid"`, both required, each at most 65535 (a syntax limit;
an id outside the host's subuid/subgid range fails at deploy). Names and unknown keys are errors.

**2. Validation at apply.** A path is absolute and normalized (no empty, `.` or `..` component,
no trailing `/`, not `/`, no NUL), unique per service, not an ancestor of another of its files,
not under `/proc`, `/sys` or `/dev`, not `/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`,
`/run/.containerenv` (podman mounts over these), `/etc/passwd` or `/etc/group` (podman may append
to them), and neither equal to, under nor an ancestor of a service `volumes` path. Inline content
is capped at 256 KiB per file and 1 MiB per stack. The apply routes' `DefaultBodyLimit` goes from
2 MB to 8 MiB; a larger body (mostly escapes) gets axum's 413, naming no service or path.

**3. Mechanism: upload between create and start.** `ContainerRuntime` gains
`image_volumes(&PinnedImage) -> Vec<String>` (the image's `Config.Volumes`; the fake models it).
In Pulling (architecture §5.1), a file equal to, under or an ancestor of one fails the deploy.
`ContainerSpec` gains `files`; `create_container` uploads them (`PUT …/archive`, layout in
architecture §8) before it returns. Each path is first resolved through the stat of its deepest
existing ancestor (`linkTarget`); the volume, image-volume and reserved-path checks run again on
the resolved path, and the upload goes there. A path that is itself a symlink, dangling or not,
fails naming the link. Image directories are never replaced (`noOverwriteDirNonDir`). On failure
the container is removed; the error is the text after podman's last `copier: put:` (else a
generic message), never host paths or file content.

**4. Ownership and mode.** Tar header ids are container ids (no rootless mapping). `content`
files default to `0:0 0444`: anyone reads, only root changes. `secret` files default to `0400`,
owned by the container's user (the stack's `user`, else the image `USER`) with podman's gid for
it (`copyUIDGID=true`). Images that drop root themselves (nginx, Mosquitto, postgres) run as
root for podman; the stack-format reference documents `owner` for them.

**5. Not writable by a non-root container user.** Root, and a secret file's owner after a chmod,
can rewrite them until the generation ends; `read_only = true` (recommended) stops that except
under `/run`, `/tmp` and `/var/tmp` (podman tmpfs, refilled from the layer at each start).

**6. Secrets (amends ADR-0029).** One `Service` accessor yields the secret references of env and
files; version collection, ADR-0049's redeploy, the missing-secret check (rejected by name and
path, never with a value), the reveal at create and exec-output redaction all read from it.
Secret files are plaintext in the container's layer, as env secrets are in its config.
Redaction also covers each trimmed line (4+ characters) of a file secret. A file secret over
256 KiB fails the deploy (values change after apply), and none counts towards the 1 MiB.

**7. Config hash.** A `files` key is hashed only when non-empty, so no existing hash changes. Per
file it holds `{path, mode, owner, sha256(content)}` or `{path, mode, owner, secret, version}`,
resolved (`"user"` for the container-user default). A change redeploys only its service.

**8. Rollback, restarts and GC.** A rollback recreates inline content exactly; secret files get
the current value (ADR-0041 Correction). Restarts and reboots keep the layer; removal is the GC.

**9. Podman 5.4 or later.** A stack with `files` is rejected at apply, and a deployment of one in
`Validating`, on an older podman, naming the running and the required version; stacks without
`files` are unaffected and doctor reports the version. SELinux hosts are best effort.

**10. UI and scope.** The stack overview lists path, mode, owner and size or secret name, never
secret content. Out of scope: templating, binary content, writable or persistent files (use a
volume), whole directories, files from URLs or host paths.

## Alternatives considered

- **(a) Read-only single-file bind mounts from `<state_dir>/files/<stack>/<svc>/g<n>/`** (`:ro,U`
  works): truly read-only, but a host path (an ADR-0019 exception) and GC on four removal paths.
  Secret files would sit in the backed-up state dir, or on a tmpfs a reboot empties.
- **(b) podman secrets with a target path:** libpod-only, a second object lifecycle, one owner.
- **(c) A per-generation volume with `Subpath`:** a lifecycle outside ADR-0030 and a helper.
- **Secret mode `0440` or `0444`:** opens the secret to the group or everyone. **Rejecting files
  under the tmpfs paths of `read_only` services:** bars `/run/secrets/`.
- **Always reading `/etc/passwd`, or as a fallback for podman 4.x:** re-implements podman's user
  lookup; Ubuntu 26.04 (5.7) and Debian 13 (5.4) need neither. **`owner = "uid"`** with
  gid 0 (root group reads) or gid = uid: an implicit gid. **Owner names:** need `/etc/passwd`.
- **No `owner`:** a second user needs a world-readable secret. **Always hashing `files`:** every
  service would redeploy once after the upgrade.

## Consequences

- ADR-0029 carries a status note: its "may come later" tmpfs file secrets are not built; secret
  files live in the container's layer. `binds` stays `None` (ADR-0019).
- Architecture §5.1 (Pulling), §5.5 and the stack-format reference (redaction), §8 and §9 change.
- The M12 epic's "read-only" criterion becomes "not writable by a non-root container user
  (read-only with `read_only = true`)"; its GC criterion is met by container removal.
- Ubuntu 24.04 (podman 4.9) and Debian 12 (4.3) cannot use `files`. Rootless 5.x and SELinux
  were not tested; on RHEL-family hosts podman's `mounts.conf` may mount over paths under
  `/run/secrets/`, hiding files uploaded there.
