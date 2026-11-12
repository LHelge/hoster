---
title: Stack file format
category: Reference
---

Every field follows from an accepted ADR.

# 1. Format choice: TOML

A stack is described in one **TOML** file, conventionally `<stack>.stack.toml`. It lives in the
app's repository and is applied through the web UI ([ADR-0027](https://github.com/LHelge/hoster/blob/main/docs/adr/0027-stacks-applied-via-ui-webhooks-carry-digests.md)).

**Why TOML:**

- **No implicit typing.** YAML's surprises cannot happen: `no` → `false`, `0755` → octal, `1.10`
  → `1.1`, unquoted `on`. Hostnames, ports and versions mean what they look like.
- **Native fit for Rust.** The `toml` crate with `serde` gives strict deserialization with
  `deny_unknown_fields`, and errors carry line and column spans. That gives good validation
  messages for little code.
- **Consistent with the rest of hoster.** hoster's own config (`hoster.toml`) and Cargo use TOML,
  so there is one format to learn.
- **Comments.** Unlike JSON, TOML has them.
- **Not compose-shaped on purpose.** A YAML file that looks like `docker-compose.yml` would invite
  people to paste compose files and expect compose semantics (`build:`, `depends_on` conditions,
  `ports:`, `privileged:`). hoster's model is different: routes instead of ports, and a deliberately narrow
  security surface ([ADR-0019](https://github.com/LHelge/hoster/blob/main/docs/adr/0019-no-container-runtime-access-for-apps.md)). A different format makes that obvious.
  [[third-party-images|Running third-party images]] translates a compose file by hand.

**Alternatives considered:**

| Format | Why not |
|---|---|
| YAML (compose-like) | Implicit typing pitfalls. The Rust YAML ecosystem has been unsettled since `serde_yaml` was archived (2024). Compose look-alike expectations. |
| JSON | No comments, noisy to write by hand. JSON *is* used on the wire (webhook, internal API). |
| KDL / RON | Nice, but unfamiliar to most people, and editor support is thinner. |
| CUE / Nickel / Dhall | Powerful validation and templating, but a heavy dependency or toolchain for a small schema. |

**Downside accepted:** deeply nested arrays of tables (`[[services.api.routes]]`) are verbose.
Inline tables are allowed where they read better.

## 1.1 Editor support: the JSON Schema

hoster publishes a JSON Schema (draft 2020-12) for stack files at
`https://docs.hoster.lhelge.se/stack.schema.json`. Editors with TOML schema support use it for
completion, hover docs and validation while you write a stack file. Each hoster also serves the
schema of the version it runs at `https://<admin host>/schema/stack-v1.json`, without a login,
and `hoster stack schema` prints it.

Name the schema in the file's **first line**:

```toml
#:schema https://docs.hoster.lhelge.se/stack.schema.json
version = 1
name = "notes"
```

`#:schema` is a [Taplo](https://taplo.tamasfe.dev) directive. To hoster it is an ordinary
comment.

- **VS Code:** install the "Even Better TOML" extension (`tamasfe.even-better-toml`), which is
  built on Taplo. The directive is enough. To cover every stack file without it, map the schema in
  `settings.json`:

  ```json
  "evenBetterToml.schema.associations": {
    ".*\\.stack\\.toml$": "https://docs.hoster.lhelge.se/stack.schema.json"
  }
  ```

- **Taplo elsewhere** (its language server in Neovim, Helix or Zed, or `taplo check` in CI): the
  directive works too, or put a rule in `.taplo.toml` at the repository root:

  ```toml
  [[rule]]
  include = ["**/*.stack.toml"]

  [rule.schema]
  url = "https://docs.hoster.lhelge.se/stack.schema.json"
  ```

- **JetBrains IDEs:** add the URL under *Settings → Languages & Frameworks → Schemas and DTDs →
  JSON Schema Mappings*, with the file path pattern `*.stack.toml`.

The schema checks each field on its own: types, required fields, unknown fields (as
`deny_unknown_fields` does), `update` values, and the rules for names, hosts, prefixes, mount
paths, environment variable and secret names, `expect_status` and durations. Rules that need more
than one value stay with hoster, which checks them when the stack is applied:

- a (host, prefix) used twice, within the stack or by another stack (§2.3);
- `depends_on` cycles, unknown services, or a service that names itself;
- secrets of another stack, a name set in both `env` and `secrets`, and a mount path used twice;
- a tag or digest on third-party images, and `track` only on CI-built `ghcr.io` images without
  one;
- `http` and `tcp` checks only on routed services and against one of their routes' ports, and a
  timings-only `health` table only on routed services.

hoster also rejects durations that overflow or whose fraction is not a whole number of
nanoseconds, such as `"0.001h"`. The apply preview stays the final word.

`hoster stack check FILE...` runs hoster's own parse offline, so it checks all of the above
except routes taken by another stack. It needs no running hoster; see
[[deploying-from-ci#2-6-check-the-stack-file-on-every-push|Deploying from CI, §2.6]] for a CI
step.

# 2. Schema

## 2.1 Top level

```toml
version = 1          # required; schema version, integer
name = "notes"       # required; stack name

[services.<service-name>]   # one or more
# ...
```

| Field | Type | Required | Rules |
|---|---|---|---|
| `version` | integer | yes | Must be `1`. |
| `name` | string | yes | `^[a-z][a-z0-9-]{0,30}$`. Unique per hoster instance. Used in network and container names (`hoster-<name>`, `hoster-<name>-<svc>-g<n>`). |
| `services` | table of service | yes | At least one. Keys are service names: `^[a-z][a-z0-9-]{0,30}$`. The key is also the DNS alias on the stack network. Apply rejects a service whose container name another stack already uses (stack `a-b` service `c` and stack `a` service `b-c` are both `hoster-a-b-c-g<n>`, [ADR-0063](https://github.com/LHelge/hoster/blob/main/docs/adr/0063-reject-cross-stack-podman-name-collisions.md)). |
| `drain_timeout` | duration | no | Default `30s`. Upper bound for draining old upstreams ([ADR-0017](https://github.com/LHelge/hoster/blob/main/docs/adr/0017-zero-downtime-deploy-sequence.md)). |
| `deploy` | table | no | Who may deploy this stack through the webhook (§2.5, [ADR-0047](https://github.com/LHelge/hoster/blob/main/docs/adr/0047-oidc-authenticated-deploy-webhook.md)). Without it, the stack can only be deployed from the UI. |

## 2.2 Service

| Field | Type | Required | Rules |
|---|---|---|---|
| `image` | string | yes | Repository reference **without a tag** for CI-built images, e.g. `ghcr.io/acme/notes-api`, whose digest comes from the deploy. Third-party images may use `repo:tag`, `repo:tag@sha256:…` or `repo@sha256:…`. A tag without a digest is **resolved to a digest when the stack is applied**, stored in the stack revision and shown in the apply diff ([ADR-0025](https://github.com/LHelge/hoster/blob/main/docs/adr/0025-registry-credentials-and-image-references.md)). |
| `track` | string | no | Tag the ghcr poller watches for this service, e.g. `"main"` or `"latest"`. Only for CI-built `ghcr.io` images. The poller deploys only when the tag's digest changes ([ADR-0043](https://github.com/LHelge/hoster/blob/main/docs/adr/0043-edge-triggered-poller.md)). |
| `auto_update` | string | no | `"off"` (default), `"digest"`, `"patch"` or `"minor"`: whether hoster applies this image's updates by itself (§2.2.4, [ADR-0061](https://github.com/LHelge/hoster/blob/main/docs/adr/0061-third-party-image-updates.md)). Only for third-party images with a tag; `"patch"` and `"minor"` need a tag that reads as a version. Not part of the config hash, so changing it redeploys nothing. |
| `command` | array of string | no | Overrides the image `CMD`. |
| `entrypoint` | array of string | no | Overrides the image `ENTRYPOINT`. |
| `env` | table string → string | no | Plain, non-secret environment variables. For a routed service with exactly one distinct container port, hoster adds `PORT=<that port>` unless `env` (or `secrets`) sets `PORT` itself ([ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md)). A service whose routes use two or more different container ports, or that has no routes, gets no `PORT`; set what it needs in `env`. The injected value is part of the service's config hash, so changing the route's `port` redeploys the service. |
| `secrets` | table string → string | no | Env var name → secret name `<stack>/<name>` in hoster's encrypted secret store ([ADR-0029](https://github.com/LHelge/hoster/blob/main/docs/adr/0029-secret-store-and-config-aware-idempotency.md)). The value never appears in the stack file. Values are injected as environment variables at container create; a deployment that references a secret which is not set is rejected, naming the secret (never its value). Replacing a secret changes its version, which is part of the service's config hash. |
| `volumes` | table string → string | no | Volume name → mount path. Named podman volumes `hoster-<stack>-<volume>` only, kept on stack removal ([ADR-0030](https://github.com/LHelge/hoster/blob/main/docs/adr/0030-named-volumes-and-documented-backups.md)). Host bind mounts are not expressible ([ADR-0019](https://github.com/LHelge/hoster/blob/main/docs/adr/0019-no-container-runtime-access-for-apps.md)). hoster creates each volume on first use, labelled `hoster.managed=true` and `hoster.stack=<stack>`, and refuses to mount an existing one labelled for another stack. Because names may contain `-`, apply also rejects a volume whose podman name another stack already uses (stack `a-b` volume `c` and stack `a` volume `b-c` are both `hoster-a-b-c`), including a volume kept from a removed stack ([ADR-0063](https://github.com/LHelge/hoster/blob/main/docs/adr/0063-reject-cross-stack-podman-name-collisions.md)). Validation warns when a service with volumes uses `update = "start-first"`, since old and new containers would mount the volume at the same time during a deploy ([ADR-0039](https://github.com/LHelge/hoster/blob/main/docs/adr/0039-per-service-update-strategy.md)). |
| `files` | table path → file | no | Config files hoster writes into each container before it starts: absolute container path → `{ content = "…" }` or `{ secret = "<stack>/<name>" }`, with optional `mode` and `owner`. See §2.2.1 ([ADR-0057](https://github.com/LHelge/hoster/blob/main/docs/adr/0057-config-files-in-stack-files.md)). Needs podman 5.4 or later. A file's content, mode and owner are part of the service's config hash, so changing one redeploys the service. |
| `health` | table | no | See §2.4 ([ADR-0038](https://github.com/LHelge/hoster/blob/main/docs/adr/0038-host-driven-health-probes-no-limits-v1.md), [ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md), [ADR-0056](https://github.com/LHelge/hoster/blob/main/docs/adr/0056-service-dependencies-and-exec-health-checks.md)). Defaults: HTTP `GET /healthz` on the first route's container port for routed services, "running for 5 s" for others. Services whose health endpoint is elsewhere (including prefix-mounted ones) declare it; a service without routes may only declare an `exec` check. |
| `depends_on` | array of string | no | Services of this stack that must pass their health check before this one starts, e.g. `["db", "cache"]` ([ADR-0056](https://github.com/LHelge/hoster/blob/main/docs/adr/0056-service-dependencies-and-exec-health-checks.md)). Each must name another service of the stack, at most once; cycles are rejected and named in order (`app → worker → app`). There are no compose-style `condition` variants. Each service starts as soon as its own dependencies pass, during deploys and after a reboot or crash. Outside a deploy, a running dependency that has not passed within min(`start_period`, 5 min) no longer holds its dependents, and a dependency that has never run is ignored. A dependency going down never stops its dependents. Not part of the config hash: changing it reorders starts but never recreates a container. The apply preview lists the start order, and the stack overview shows each service's dependencies and "waiting for db" while the reconciler holds it back. A worked example is in §3.2. |
| `update` | string | no | `"start-first"` (default; zero-downtime) or `"stop-first"` (for services that cannot run twice; short outage with 503 + `Retry-After`) ([ADR-0039](https://github.com/LHelge/hoster/blob/main/docs/adr/0039-per-service-update-strategy.md)). |
| `stop_timeout` | duration | no | Default `10s`. SIGTERM → SIGKILL grace period. |
| `user` | string | no | `uid[:gid]` inside the container. |
| `read_only` | bool | no | Default `false`. Read-only root filesystem. podman then mounts its own unsized tmpfs on `/tmp`, `/run` and `/var/tmp`; see §2.2.2. |
| `shm_size` | size | no | Size of `/dev/shm`, at least `"1MiB"`, e.g. `"256MiB"` (§2.2.2, [ADR-0059](https://github.com/LHelge/hoster/blob/main/docs/adr/0059-runtime-options-and-resource-limits.md)). Default podman's 65536000 bytes (62.5 MiB). |
| `tmpfs` | table path → size | no | In-memory directories: absolute container path → size, each at least `"1MiB"`, e.g. `{ "/tmp" = "64MiB" }` (§2.2.2, [ADR-0059](https://github.com/LHelge/hoster/blob/main/docs/adr/0059-runtime-options-and-resource-limits.md)). Mounted `rw,nosuid,nodev,mode=1777`. |
| `init` | bool | no | Default `false`. Runs podman's init (catatonit) as PID 1 (§2.2.2, [ADR-0059](https://github.com/LHelge/hoster/blob/main/docs/adr/0059-runtime-options-and-resource-limits.md)). |
| `limits` | table | no | Resource limits `memory`, `pids` and `cpus`, each optional, with no defaults (§2.2.3, [ADR-0059](https://github.com/LHelge/hoster/blob/main/docs/adr/0059-runtime-options-and-resource-limits.md)). The host must delegate their cgroup controllers. |
| `routes` | array of route | no | Zero or more. If empty, the service publishes no ports ([ADR-0007](https://github.com/LHelge/hoster/blob/main/docs/adr/0007-route-model-and-loopback-only-publishing.md)). |

There are intentionally **no** fields for `ports`, `privileged`, `network_mode`, `cap_add`,
`devices`, bind mounts, or the podman socket ([ADR-0007](https://github.com/LHelge/hoster/blob/main/docs/adr/0007-route-model-and-loopback-only-publishing.md), [ADR-0019](https://github.com/LHelge/hoster/blob/main/docs/adr/0019-no-container-runtime-access-for-apps.md)).
Nor for the other runtime settings compose offers (§2.2.2).

## 2.2.1 Config files ([ADR-0057](https://github.com/LHelge/hoster/blob/main/docs/adr/0057-config-files-in-stack-files.md))

Many images are configured by a file, not by environment variables. Host bind mounts are not
expressible, so the stack file carries the file itself:

```toml
[services.prometheus.files]
"/etc/prometheus/prometheus.yml" = { content = """
global:
  scrape_interval: 15s
""" }
"/etc/prometheus/web.yml" = { secret = "monitoring/web-config", mode = "0440", owner = "0:65534" }
```

Each key is an absolute path inside the container. hoster writes the files into every new
container of the service after it is created and before it starts. They live in the container's
own layer: a restart or a reboot keeps them, and they go away with the container. There is no
directory on the host to manage or back up.

| Field | Type | Required | Default | Rules |
|---|---|---|---|---|
| `content` | string | one of the two | — | The file's text. A TOML multi-line string (`"""…"""`, or `'''…'''` to keep backslashes as written) holds a whole file; the newline right after the opening quotes is dropped. At most 256 KiB per file, and 1 MiB for all `content` of a stack together. `content = ""` is an empty file. |
| `secret` | string | one of the two | — | A secret `<stack>/<name>` of this stack ([ADR-0029](https://github.com/LHelge/hoster/blob/main/docs/adr/0029-secret-store-and-config-aware-idempotency.md)) whose value becomes the file's content. Set it on the stack's environment tab, where the file is listed with its secret. At most 256 KiB, checked when the service deploys. |
| `mode` | string | no | `"0444"` for `content`, `"0400"` for `secret` | Four octal digits from `"0000"` to `"0777"`. No setuid, setgid or sticky bit. |
| `owner` | string | no | `"0:0"` for `content`; the container's user for `secret` | Numeric `"uid:gid"` inside the container, both required, each at most 65535. Names are not accepted. |

**Paths.** A path is absolute and written exactly one way: no `.`, `..` or empty component and no
trailing `/`. hoster creates missing parent directories, owned by root with mode `0755`. A file
replaces a file the image has at that path, but never a directory or a symbolic link. A path may
not be:

- in, or a parent directory of, one of the service's `volumes`, or a volume the image declares
  (`VOLUME`); the image's volumes are known once it is pulled, so that check fails the deploy,
  not the apply;
- a parent directory of another file of the service;
- in `/proc`, `/sys` or `/dev`;
- one of `/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`, `/etc/passwd`, `/etc/group` or
  `/run/.containerenv`, which podman writes itself.

Paths are followed through the image's symbolic links (`/var/run` is `/run` in many images), and
the rules above apply to where a file really lands.

**Who can read and change a file.** With the defaults, everyone in the container reads a
`content` file and only root can change it, and a `secret` file is readable by the user the
container runs as (the service's `user`, else the image's) and nobody else. A container that runs
as root can rewrite any of them until it is replaced. Set `read_only = true` to make them
read-only for everyone; the exceptions are `/run`, `/tmp` and `/var/tmp`, which stay writable
for root because podman mounts a tmpfs there and refills it from the files at each start.

**Images that start as root and switch user themselves** (nginx, postgres, Mosquitto) run as root
as far as podman knows, so a `secret` file is owned by root with mode `0400` and the process that
reads it, after the switch, is refused. Give such a file to the user the image switches to, e.g.
`owner = "101:101"` for nginx, or widen its `mode`.

**Changes and rollback.** Changing a file's `content`, `mode` or `owner`, adding or removing a
file, or setting its secret to a new value redeploys that service and no other; the apply preview
names the file next to the service (`redeploy prometheus`, `file
/etc/prometheus/prometheus.yml changed`). Rolling back
restores the `content` of the revision rolled back to. A `secret` file gets the secret's current
value: secrets are not versioned with revisions.

**Secrets in files.** A deployment whose file names a secret that is not set is rejected, naming
the secret and the path. A secret file is plaintext inside the container's layer in the hoster
user's podman storage, as an environment secret is in the container's configuration. Health check
output is redacted for the whole value and for each of its lines of four characters or more.

**Requirements and limits.**

- podman 5.4 or later. On an older podman (Ubuntu 24.04 ships 4.9, Debian 12 ships 4.3) a stack
  with `files` is rejected when it is applied, naming both versions; stacks without `files` are
  unaffected. `hoster doctor` shows the podman version.
- Text only: there is no templating, no binary content, no whole directories and no files
  fetched from a URL or the host. A file the app must write to, or keep, belongs in a volume.
- On RHEL-family hosts podman's own `mounts.conf` can mount host directories over
  `/run/secrets/`, hiding files written there. Use another directory on those hosts.

A worked example is in §3.3.

## 2.2.2 Shared memory, tmpfs and init ([ADR-0059](https://github.com/LHelge/hoster/blob/main/docs/adr/0059-runtime-options-and-resource-limits.md))

Some images need more of the container than its file system. Postgres outgrows the default
`/dev/shm`, an image with `read_only = true` still writes a cache or a PID file somewhere, and a
process that starts children without reaping them needs an init. Three service fields cover
that:

```toml
[services.db]
image = "docker.io/library/postgres:17"
shm_size = "256MiB"          # /dev/shm; podman's default is 62.5 MiB

[services.web]
image = "docker.io/nginxinc/nginx-unprivileged:1.27"
read_only = true
tmpfs = { "/var/cache/nginx" = "16MiB", "/tmp" = "8MiB" }
init = true                  # catatonit as PID 1
```

**Sizes** are strings: a whole number and a unit, with no space and no fraction. The units are
`B`, `KB`, `MB` and `GB` (1000, 1000², 1000³ bytes) and `KiB`, `MiB` and `GiB` (1024, 1024²,
1024³ bytes), and they are case-sensitive. A bare number (`512` or `"512"`) and Docker's `512m`
are errors that suggest `"512MiB"`: `m` doesn't say whether it means 1000² or 1024². A size is
at most 9223372036854775807 bytes. `shm_size` and every `tmpfs` size are at least `"1MiB"`,
so `"0B"` never means "podman's default".

**`shm_size`** sizes `/dev/shm`. Without it podman gives 65536000 bytes (62.5 MiB), too little
for Postgres's parallel queries (`could not resize shared memory segment`).

**`tmpfs`** mounts a directory in memory at each path, at most its size. It is empty at every
start, so it holds scratch data only: anything to keep belongs in a volume. Each mount is
`rw,nosuid,nodev,mode=1777`, executable and writable by every user in the container. The mode
matters for a directory the image already has: without it the mount comes out owned by root
with the image directory's mode, and an image that runs as another user (nginx-unprivileged's
`/var/cache/nginx`, uid 101) could not write there. A path is absolute and written one way (no
`.`, `..` or empty segment, no trailing `/`, no `:` or `,`), and may not be:

- `/`, in `/proc`, `/sys` or `/dev`, which podman mounts over (`/dev/shm` is sized by
  `shm_size`);
- at, in or a parent directory of one of the service's `volumes`, `files` or other `tmpfs`
  paths.

**With `read_only = true`** podman mounts its own unsized tmpfs on `/tmp`, `/run` and
`/var/tmp`, so those three stay writable without a `tmpfs` entry. Declaring one of them replaces
podman's mount at that path only: `tmpfs = { "/tmp" = "64MiB" }` caps `/tmp`, and `/run` and
`/var/tmp` stay podman's, without a size. Shared memory and tmpfs pages count as the
container's memory.

**`init = true`** runs podman's init, catatonit, as PID 1. It passes signals on to the image's
process and reaps the exited child processes that an app running as PID 1 often leaves as
zombies. `init = false` is the same as leaving it out. The host needs the `catatonit` package,
which [[install|install.sh]] installs with `apt-get` or `dnf` (elsewhere, see
[[install#cgroup-delegation-and-catatonit|Install]]): without it the container cannot be created and
the deploy fails. podman bind-mounts the init binary into the container itself, the
only host file a stack file can bring in.

A change to any of the three is part of the service's config hash, so it recreates the
service's containers through a normal deploy. A service without them keeps the hash it had
before these fields existed.

**Not expressible.** Other runtime settings stay unknown fields, most because they reach into
the host or the kernel ([ADR-0019](https://github.com/LHelge/hoster/blob/main/docs/adr/0019-no-container-runtime-access-for-apps.md)): `stop_signal`, `cap_add`, `cap_drop`, `sysctls`,
`devices`, `privileged`, `ulimits`, `security_opt`, the `ipc` and `pid` modes, OOM-killer tuning,
cpusets, I/O limits, reservations and CPU shares. Each can be added on its own once an image
needs it. There is no `noexec` tmpfs and no per-mount option.

## 2.2.3 Resource limits ([ADR-0059](https://github.com/LHelge/hoster/blob/main/docs/adr/0059-runtime-options-and-resource-limits.md))

Without limits a container can take all of the host's memory, CPU time and processes, and with
them hoster's and the other stacks'. A service's `limits` table caps them:

```toml
[services.db]
image = "docker.io/library/postgres:17"
shm_size = "256MiB"

[services.db.limits]
memory = "1GiB"   # hard limit, no swap: OOM-killed above it
pids = 512        # processes and threads
cpus = 1.5        # one and a half CPUs' worth of time
```

| Key | Type | Rules |
|---|---|---|
| `memory` | size | At least `"8MiB"`, written like `shm_size` (§2.2.2). A hard limit: the container gets **no swap**, and the kernel's OOM killer ends a process in it that would go over. |
| `pids` | integer | 1 to 4294967295. The most processes and threads the container may have at once; a `fork` past it fails. |
| `cpus` | number | 0.01 to 1024, an integer or a float such as `1.5`. CPU time per scheduling period: `1.5` is one and a half CPUs' worth, spread over any CPUs. |

There are **no defaults**: each limit that is left out keeps podman's (no memory or CPU cap,
2048 processes), and `limits = {}` is the same as no table. Other keys (`memory_swap`,
reservations, CPU shares) are unknown fields.

**What counts.** `memory` covers everything in the container's memory: shared memory
(`shm_size`), every `tmpfs`, and with `read_only = true` podman's own tmpfs on `/tmp`, `/run` and
`/var/tmp`. Validation warns when `shm_size` and the `tmpfs` sizes add up to `memory` or more.
`pids` covers `init`'s catatonit and every `exec` health check while it runs, as well as the app.

**Start-first deploys run old and new together.** During a start-first deploy the new container
starts next to the old one and both run until the swap, each with its own limits. So for a moment
a service can use twice its limits; plan the host's memory for that, or use `stop-first`. hoster
reserves nothing for itself: leave the host some memory and CPU beyond the sum of the limits for
hoster, its proxy and podman.

**The host must delegate the controllers.** Rootless podman can only enforce a limit whose cgroup
controller (`memory`, `pids`, `cpu`) systemd delegates to hoster's user manager,
`user@<uid>.service`. A stack that sets a limit on a missing controller is rejected when it is
applied, and a deployment of a stored revision (a rollback, a CI deploy) is rejected in
`Validating`, naming the controller and the fix (Install, [[install#cgroup-delegation-and-catatonit|cgroup
delegation]]). If hoster cannot read the user manager's `cgroup.controllers`, only stacks that set
a limit are rejected. The editor's live check marks the limit itself. A `cpus` above the host's
CPU count or a `memory` above its total memory caps nothing, and applying warns about it.
The stack's overview shows each running container's CPU and memory against its service's
`cpus` and `memory`, with a meter that turns orange from 80 % and red from 95 %
([[overview#resource-usage|Resource usage]]).

A change to a limit is part of the service's config hash (`memory` by bytes, `cpus` by the
nano-CPUs podman gets), so it recreates the service's containers through a normal deploy. A
service without limits keeps the hash it had before limits existed.

## 2.2.4 Updates of third-party images ([ADR-0061](https://github.com/LHelge/hoster/blob/main/docs/adr/0061-third-party-image-updates.md))

hoster checks every tagged third-party image for updates in the background, every 6 hours by
default (Settings → Updates). It looks for two things:

- **A moved tag:** the tag now points at a different digest. Floating tags such as `16`,
  `8-alpine` or `latest` get their rebuilds and patch releases this way.
- **A newer version:** the registry lists a higher version in the same **line**. A tag is a
  version when it is an optional `v`, one to three numbers separated by `.`, and an optional
  `-suffix`. Two tags are in the same line when they have the same `v` style, the same number of
  parts and the same suffix: `16.4-alpine` → `16.6-alpine` is an update, but `17.0`,
  `16.6-bookworm` and `16.4.1-alpine` are not. Pre-releases (a suffix with `rc`, `beta`,
  `alpha`, `dev`, `nightly`, `preview` or `snapshot`) are never offered.

The parts count by position: the first is the major, the second the minor and the third the
patch. So `2.3.1` → `2.3.4` is a patch, `2.3.1` → `2.4.0` a minor and `2.3.1` → `3.0.0` a major.
A tag with fewer parts gets the rest as moved tags: `16.4` → `16.5` is a minor, and `16.4`'s
patch releases come as new digests of `16.4`.

Available updates are shown on the stack page whatever the policy, majors marked as such, and one
click opens the usual apply preview. `auto_update` decides what hoster applies **by itself**:

| Value | Applies |
|---|---|
| `"off"` (default) | Nothing; updates are only shown. |
| `"digest"` | Moved tags. |
| `"patch"` | Moved tags and newer patch versions. On a tag with one or two parts this is the same as `"digest"`. |
| `"minor"` | Moved tags and newer minor and patch versions. |

A major is never applied automatically. Change the tag yourself, after reading the release notes
(for a database, see [[third-party-images#4-6-updating-a-database-to-a-new-major-version|updating a
database to a new major version]]).

```toml
[services.db]
image = "docker.io/library/postgres:17"   # floating major: patch releases are new digests
auto_update = "digest"

[services.server]
image = "ghcr.io/immich-app/immich-server:v2.3.1"
auto_update = "minor"

[services.ml]
image = "ghcr.io/immich-app/immich-machine-learning:v2.3.1"
auto_update = "minor"
```

**Services that share a release move together.** In a stack, services whose images have the same
registry, namespace and tag form a group, like `server` and `ml` above. A new version is offered
for a group only once every member's repository has it, and the group is applied under the
strictest policy among its members: one member set to `"off"` keeps the whole group shown but not
applied.

**How it is applied.** An automatic update writes a new revision with the new tag, keeping the
file's comments and formatting, and deploys it like any apply. Everything a check finds for one
stack goes into one revision and one deployment. A stack that is already deploying is left for the
next check. A target is tried once: if its deployment fails, or you roll back or edit the tag
back, hoster leaves it alone until a newer one appears.

**Risks.** hoster only sees what the registry shows. A moved tag may be a rebuild with other
packages inside, and a minor version can still break your app if the image doesn't follow its
own versioning. `"minor"` means trusting the image's maintainers; prefer `"patch"` or `"digest"`
for anything holding data, and keep backups ([[backup]]).

## 2.3 Route

| Field | Type | Required | Default | Rules |
|---|---|---|---|---|
| `host` | string | yes | — | A lowercase FQDN, without port or scheme. No wildcards ([ADR-0016](https://github.com/LHelge/hoster/blob/main/docs/adr/0016-persist-acme-state-defer-dns01.md)). Must resolve to the VPS for ACME. |
| `prefix` | string | no | `"/"` | Must start with `/`. No trailing `/` except the root. No `?`, `#`, `//`, `.` or `..` segments. Normalized into segments for matching and collision checks ([ADR-0009](https://github.com/LHelge/hoster/blob/main/docs/adr/0009-host-and-longest-segment-prefix-routing.md)). |
| `strip_prefix` | bool | no | `false` | Remove the matched prefix before forwarding ([ADR-0010](https://github.com/LHelge/hoster/blob/main/docs/adr/0010-prefix-preservation-and-forwarded-headers.md)). Has no effect on `/`; validation warns if it is set there. |
| `port` | integer | yes | — | Container port, 1–65535, i.e. the port the app listens on inside the container. hoster allocates the host port and binds it on `127.0.0.1`; the app never sees it. **The app must bind `0.0.0.0` or `::`, not `127.0.0.1`:** rootless port forwarding cannot reach a loopback-bound listener. Single-port services also receive it as `PORT` (see `env`). Host-level redirects (`www` → apex) are not expressible in v1 ([ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md)); declare both hosts as routes or redirect in the app. |
| `timeouts` | table | no | hoster defaults | `connect`, `read`, `write`, `idle` as durations ([ADR-0011](https://github.com/LHelge/hoster/blob/main/docs/adr/0011-streaming-bodies-and-per-route-timeouts.md)). `read` and `write` also bound how long a client may stall while sending a request body or reading a response, so a route with long-lived streams (WebSockets, server-sent events) raises them. |

Every routed host is served over HTTPS only, and port 80 redirects to it. hoster does not add
`Strict-Transport-Security` to an app's responses: whether a host wants HSTS, and for which
subdomains, is the app's choice, and the app can send the header itself. (hoster's own admin
host sends it.)

**Validation (a deploy or apply fails as a whole):**

- Duplicate (host, prefix) inside the stack → error.
- (host, prefix) owned by another stack → error ([ADR-0012](https://github.com/LHelge/hoster/blob/main/docs/adr/0012-reject-cross-stack-route-collisions.md)).
- More than one service of the same stack on the same (host, prefix) → error, same as a duplicate.
- Durations parse as humantime strings: `"500ms"`, `"30s"`, `"5m"`.

## 2.4 Health check (ADR-0038, ADR-0056)

```toml
[services.api.health]
http = { path = "/healthz", port = 8080, expect_status = "2xx" }
# or: tcp = { port = 8080 }
# or: exec = ["pg_isready", "-U", "app"]
interval = "1s"
timeout = "2s"
healthy_after = 2     # consecutive successes required
start_period = "30s"  # give up and abort the deploy after this
```

**Which check a service gets** (ADR-0056), in this order:

1. The probe its `health` table declares: `http`, `tcp` or `exec`. `http` and `tcp` need a route;
   `exec` works on any service and is the only probe a service without routes may declare. A
   timings-only table on a routed service tunes the default `GET /healthz` probe.
2. Without a `health` table, a routed service gets `GET /healthz` on its first route's container
   port, expecting 2xx–3xx.
3. Without a `health` table or routes, "running and not restarted for 5 s".

The image's own `HEALTHCHECK` is never used, and podman never runs it either: hoster creates every
container with health checks off.

hoster runs `http` and `tcp` probes from the host against the container's allocated `127.0.0.1`
port. Without a `health` table, a routed service is probed with `GET /healthz` on its first
route's container port, expecting 2xx–3xx ([ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md));
set `health.http.path` when the endpoint differs, e.g. `/api/healthz` for a service mounted under
a preserved prefix. Services without routes have no loopback port. Without a `health` table they
count as healthy once "running and not restarted for 5 s"; a `health` table on such a service must
use `exec`, since `http` and `tcp` need a route.

Set at most one of `http`, `tcp` and `exec`. `exec` runs a command inside the container
([ADR-0056](https://github.com/LHelge/hoster/blob/main/docs/adr/0056-service-dependencies-and-exec-health-checks.md)): a non-empty array whose first element names the program, run through podman's
exec API without a shell, as the container's user and with its environment. Exit 0 passes; any
other exit (a shell's 126 or 127 included) is an ordinary failure. If podman cannot execute the
program at all, a deploy aborts at once with podman's error. Use `exec = ["sh", "-c", "…"]` when
the check needs a shell. On a routed service an `exec` check also requires a TCP connect to the
first route's port. Its output tail (the last 256 characters, with the service's secret values
redacted, and each line of a secret used as a config file, §2.2.1) is
recorded in the deployment's events. A probe fails at `timeout` whether or not the command has
ended; hoster then ends it if it can, and otherwise leaves it to finish. With three such checks
still running in a container, a deploy waits for one of them to end (until `start_period` runs
out) and the monitor reports the service unhealthy ("check stuck") without starting another.
Exec probes of one container never overlap.

Unset timings default to `interval = "1s"`, `timeout = "2s"` (`"5s"` for `exec`),
`healthy_after = 1` and `start_period = "30s"`. A timings-only `health` table, without a probe,
is allowed only on a routed service, where it tunes the default HTTP probe.

`path` is sent as written, so spaces, control and non-ASCII characters must be percent-encoded.
HTTP probes do not follow redirects, so a 3xx answer is the result. A deploy aborts as soon as the
container exits during the check. Each deployment's events name the check a service gets and
why (e.g. "checking db with exec: pg_isready -U app, declared in health.exec"), then its probe
results (each change, and the verdict). After a deploy, the health monitor probes live services
with the same check every 10–30 s; for `exec` it records the exit code only, never the output, and
a program podman cannot execute makes the service unhealthy. The stack overview shows the check
in effect under each service's state, with its command or path and whether it is declared or the
default, and its last result in the state's tooltip. On a deployment's page the start waves, the
check each service gets and the probe results are tagged `wave`, `check` and `probe`, with an
exec's output collapsed under its result.

## 2.5 Deploy authorization (ADR-0047)

```toml
[deploy]
repository_id = 123456789                # required: GitHub's numeric repo id (stable across renames)
refs = ["refs/tags/v*"]                  # optional: glob patterns matched against the token's `ref`
environment = "production"               # optional: must equal the token's `environment`
job_workflow_ref = "acme/notes/.github/workflows/release.yml@refs/heads/main"  # optional
```

| Field | Type | Required | Rules |
|---|---|---|---|
| `repository_id` | integer | yes | Compared with the OIDC token's `repository_id` claim. The Refused webhook deploys table on the stack's overview tab shows the claims of rejected attempts, which helps find it. |
| `refs` | array of string | no | Globs: `*` matches within one path segment, `**` across segments. At least one must match `ref`. |
| `environment` | string | no | Exact match on the `environment` claim. Pair it with GitHub environment protection rules (required reviewers, tag-only deployment). |
| `job_workflow_ref` | string | no | Exact match. Use it to require a specific reusable or release workflow. |

A webhook may only deploy services whose `image` is in a repository the token's repo publishes
to. The digest-ownership check from [ADR-0042](https://github.com/LHelge/hoster/blob/main/docs/adr/0042-webhook-endpoint-payload-and-replay.md) still applies.

# 3. Worked example: Rust API at `/api`, nginx frontend at `/`, rustfs on `s3.`

`notes.stack.toml` in the `acme/notes` repository:

```toml
version = 1
name = "notes"
drain_timeout = "30s"

# Only release tags from acme/notes, through the protected "production" environment,
# may deploy this stack through the webhook (ADR-0047).
[deploy]
repository_id = 123456789          # illustrative id of github.com/acme/notes
refs = ["refs/tags/v*"]
environment = "production"

# ─── Rust backend ────────────────────────────────────────────────────────────
[services.api]
image = "ghcr.io/acme/notes-api"   # digest supplied by the release webhook
track = "main"                     # ADR-0043: poller fallback watches this tag

[services.api.env]
RUST_LOG = "info"
BIND_ADDR = "0.0.0.0:8080"
# The API talks to object storage over the stack network, by service name:
S3_ENDPOINT = "http://storage:9000"
# Presigned URLs for browsers must use the public, dedicated hostname (ADR-0013):
S3_PUBLIC_ENDPOINT = "https://s3.notes.example.com"
S3_BUCKET = "attachments"

[services.api.secrets]             # ADR-0029: names in hoster's secret store
S3_ACCESS_KEY = "notes/s3-access-key"
S3_SECRET_KEY = "notes/s3-secret-key"
DATABASE_URL = "notes/database-url"

[services.api.health]              # ADR-0038
http = { path = "/api/healthz", port = 8080 }
start_period = "30s"


# The prefix is preserved (the default): the axum router is nested under /api
# and sees /api/notes. X-Forwarded-Prefix: /api is set as well. With
# strip_prefix = true the backend would see /notes instead.
[[services.api.routes]]
host = "notes.example.com"
prefix = "/api"
port = 8080
timeouts = { read = "60s", idle = "120s" }

# ─── nginx-served frontend ───────────────────────────────────────────────────
[services.web]
image = "ghcr.io/acme/notes-web"   # nginx + built static assets, built in CI
track = "main"

[services.web.health]
http = { path = "/", port = 80 }

[[services.web.routes]]
host = "notes.example.com"         # prefix defaults to "/"; /api is longer and wins
port = 80

# ─── rustfs (S3-compatible storage) on its own hostname ─────────────────────
[services.storage]
# Third-party image: the tag is resolved to a digest when the stack is applied (ADR-0025).
# Pinning ("…:1.0.0@sha256:…") is also allowed.
image = "docker.io/rustfs/rustfs:1.0.0"
update = "stop-first"              # ADR-0039: exclusive access to its volume

[services.storage.env]
RUSTFS_VOLUMES = "/data"           # illustrative; check the rustfs image docs

[services.storage.secrets]
RUSTFS_ACCESS_KEY = "notes/s3-access-key"
RUSTFS_SECRET_KEY = "notes/s3-secret-key"

[services.storage.volumes]         # ADR-0030: named podman volume hoster-notes-data
data = "/data"

[services.storage.health]
tcp = { port = 9000 }

# Dedicated hostname at "/": no path rewriting, so SigV4 signatures and
# presigned URLs stay valid (ADR-0013). Use path-style bucket addressing.
[[services.storage.routes]]
host = "s3.notes.example.com"
port = 9000
timeouts = { read = "10m", write = "10m", idle = "10m" }   # large objects, streamed (ADR-0011)
```

## 3.1 What hoster does with it

**Podman objects** (generation 7 shown, ports from the allocator):

| Object | Value |
|---|---|
| Network | `hoster-notes` |
| `hoster-notes-api-g7` | alias `api`, publishes `127.0.0.1:20001 → 8080/tcp` |
| `hoster-notes-web-g7` | alias `web`, publishes `127.0.0.1:20002 → 80/tcp` |
| `hoster-notes-storage-g7` | alias `storage`, publishes `127.0.0.1:20003 → 9000/tcp`, volume `hoster-notes-data:/data` |

**Routing table entries** (sorted longest prefix first per host):

| Host | Prefix | Upstream | Strip | X-Forwarded-Prefix |
|---|---|---|---|---|
| `notes.example.com` | `/api` | `127.0.0.1:20001` | no | `/api` |
| `notes.example.com` | `/` | `127.0.0.1:20002` | — | (not set) |
| `s3.notes.example.com` | `/` | `127.0.0.1:20003` | — | (not set) |

**Example requests:**

| Request | Goes to | Path the container sees |
|---|---|---|
| `GET https://notes.example.com/` | web | `/` |
| `GET https://notes.example.com/app.js` | web | `/app.js` |
| `GET https://notes.example.com/api/notes?limit=10` | api | `/api/notes?limit=10`, `Host: notes.example.com` |
| `GET https://notes.example.com/apiary` | web (segment boundary, [ADR-0009](https://github.com/LHelge/hoster/blob/main/docs/adr/0009-host-and-longest-segment-prefix-routing.md)) | `/apiary` |
| `PUT https://s3.notes.example.com/attachments/abc.png` | storage | `/attachments/abc.png`, `Host: s3.notes.example.com`, signature intact |

**Certificates:** one each for `notes.example.com` and `s3.notes.example.com`, through HTTP-01.

**Deploys:**

- A webhook carrying a new `api` digest replaces only `api` (start-first). `web` and `storage`
  are untouched, because their digests are unchanged.
- Applying a revision whose `storage` tag resolves to a new digest (or pressing "re-resolve
  tags") redeploys `storage` stop-first, which causes a short storage outage. The file says so
  explicitly ([ADR-0039](https://github.com/LHelge/hoster/blob/main/docs/adr/0039-per-service-update-strategy.md)).

## 3.2 Worked example: an app with postgres and redis

A generic web app built in CI, a background worker from the same repository, and two third-party
images it needs before it can start. `shop.stack.toml`:

```toml
version = 1
name = "shop"

# ─── The app: CI-built, routed, zero-downtime ────────────────────────────────
[services.app]
image = "ghcr.io/acme/shop"
depends_on = ["db", "cache"]              # started once both pass their checks
env = { REDIS_URL = "redis://cache:6379" }
secrets = { DATABASE_URL = "shop/database-url" }

[[services.app.routes]]
host = "shop.example.com"
port = 8080

# The app's readiness endpoint is not /healthz, so it is declared.
[services.app.health]
http = { path = "/ready" }
start_period = "60s"                      # migrations run at start-up

# ─── A worker: no routes, no health table ────────────────────────────────────
# Its check is the default for an unrouted service: running for 5 s.
[services.worker]
image = "ghcr.io/acme/shop-worker"
depends_on = ["db", "cache"]
env = { REDIS_URL = "redis://cache:6379" }
secrets = { DATABASE_URL = "shop/database-url" }

# ─── PostgreSQL ──────────────────────────────────────────────────────────────
# Two postgres processes must never share a data directory, so stop-first.
[services.db]
image = "docker.io/library/postgres:17"
update = "stop-first"
stop_timeout = "30s"                      # time for a clean shutdown checkpoint
env = { POSTGRES_USER = "shop", POSTGRES_DB = "shop" }
secrets = { POSTGRES_PASSWORD = "shop/db-password" }
volumes = { pgdata = "/var/lib/postgresql/data" }

# Not routed, so only exec can look inside. -h 127.0.0.1 skips the socket-only
# server the image runs while it initialises a new database.
[services.db.health]
exec = ["pg_isready", "-h", "127.0.0.1", "-U", "shop", "-d", "shop"]
start_period = "2m"                       # the first start initialises the database

# ─── Redis ───────────────────────────────────────────────────────────────────
# One writer per append-only file, so stop-first as well.
[services.cache]
image = "docker.io/library/redis:7"
update = "stop-first"
command = ["redis-server", "--appendonly", "yes"]
volumes = { redis = "/data" }

# Redis answers PING with LOADING while it replays the file: wait for PONG.
[services.cache.health]
exec = ["sh", "-c", "redis-cli ping | grep -q PONG"]
```

Set the secrets `shop/database-url` (`postgres://shop:…@db:5432/shop`) and `shop/db-password`
before the first deploy.

**Which checks run:** `app` gets its declared `GET /ready` on port 8080, `db` and `cache` their
declared `exec` commands, run inside the container as its user with its environment, and `worker`
the default "running for 5 s". The stack overview shows each one under the service's state.

**First apply.** Nothing runs yet. `app` and `worker` are CI-built and have no digest until CI
sends one, so the first apply deploys only `cache` and `db`. Neither has a previous generation, so
stop-first opens no gap: they start together, next to nothing. The apply preview's start order
reads `1 start cache, db`. The deployment's page logs the wave (`starting cache, db`), the check
each service gets (`checking db with exec: pg_isready …`) and the `exec` output of each failing
probe, collapsed. A slow first database initialisation is fine up to `db`'s `start_period`.

**The first webhooks.** `app` and `worker` each start on their own first webhook (or poller)
digest. That deploy changes only that service. `db` and `cache` are unchanged, so it first checks
that both run (`db (unchanged) is running; its monitor status is healthy`), and fails at once with
`db (unchanged) is not running` otherwise, instead of probing an app that cannot reach its
database. Every later webhook for `app` works the same way: only `app` changes, and it starts
first, as before.

**A new postgres tag** (applied, or "Re-resolve tags"). `db` is stop-first with a previous
generation, so it is replaced: stopped, started, probed. `app` and `worker` are unchanged and keep
running; if one exits while the database is down, hoster starts it again once the new `db` has
passed, and probes it before the deployment goes live.

**`app` and `db` change together** (an apply that edits both). `db` is replaced first. `app` is
held back until the new `db` has passed, so it is never probed against the old database; the
preview reads `1 replace db`, `2 start app held back until db is replaced and passes`. Its old generation keeps serving
until the switch, but its start and probe add to `db`'s downtime.

**After a reboot or a crash.** The reconciler starts `db` and `cache`, then each of `app` and
`worker` once both of its dependencies run and have passed their checks. Until then the overview
shows them as "waiting for cache, db". A dependency that runs but has not passed within
min(`start_period`, 5 min) stops holding them back.

## 3.3 Worked example: an image configured by files

A reverse proxy that reads its configuration from a file and a credentials file from a secret.
Nothing here is specific to the image: hoster only writes files.

```toml
version = 1
name = "edge"

[services.proxy]
image = "docker.io/library/nginx:1.27"

[services.proxy.files]
# Replaces the image's default site.
"/etc/nginx/conf.d/default.conf" = { content = """
server {
    listen 8080;
    location /healthz { return 204; }
    location / {
        auth_basic "edge";
        auth_basic_user_file /etc/nginx/htpasswd;
        root /usr/share/nginx/html;
    }
}
""" }
# nginx starts as root and its workers run as uid 101: root owns the file, group 101 reads it.
"/etc/nginx/htpasswd" = { secret = "edge/htpasswd", mode = "0440", owner = "0:101" }

[[services.proxy.routes]]
host = "edge.example.com"
port = 8080
```

**Before the first apply**, set the secret `edge/htpasswd` on the stack's environment tab, or
with the stack applied and its deployment rejected for the missing secret, set it there and the
service deploys. The value is the whole file: one `user:hash` line per user.

**What the overview shows.** The Config files card lists both files with their mode and owner:
`/etc/nginx/conf.d/default.conf` as `inline` with its size, `/etc/nginx/htpasswd` with the
secret's name. Neither file's content is shown anywhere but in the stack file's own diff.

**Changing the config.** Edit `content` and apply: the preview reads `redeploy proxy`, noting
`file /etc/nginx/conf.d/default.conf changed`, and a new container starts with the new file next
to the old one. If nginx rejects the config, the new container never passes its health check, the
deployment aborts and the old container keeps serving.

**Changing the password.** Set `edge/htpasswd` to a new value on the environment tab: the
confirmation names `proxy` as the service to redeploy.

# 4. Versioning

`version = 1` is required. Breaking schema changes bump the version. hoster rejects unknown fields
(`deny_unknown_fields`), so typos fail loudly instead of being ignored. A compatibility policy for
older versions is decided when version 2 exists.

The JSON Schema (§1.1) is generated from the parser's own types, so a release that adds a field
also adds it to the schema. The admin host serves the schema of the version it supports as
`/schema/stack-v<version>.json`.
