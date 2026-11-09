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

**Alternatives considered:**

| Format | Why not |
|---|---|
| YAML (compose-like) | Implicit typing pitfalls. The Rust YAML ecosystem has been unsettled since `serde_yaml` was archived (2024). Compose look-alike expectations. |
| JSON | No comments, noisy to write by hand. JSON *is* used on the wire (webhook, internal API). |
| KDL / RON | Nice, but unfamiliar to most people, and editor support is thinner. |
| CUE / Nickel / Dhall | Powerful validation and templating, but a heavy dependency or toolchain for a small schema. |

**Downside accepted:** deeply nested arrays of tables (`[[services.api.routes]]`) are verbose.
Inline tables are allowed where they read better.

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
| `services` | table of service | yes | At least one. Keys are service names: `^[a-z][a-z0-9-]{0,30}$`. The key is also the DNS alias on the stack network. |
| `drain_timeout` | duration | no | Default `30s`. Upper bound for draining old upstreams ([ADR-0017](https://github.com/LHelge/hoster/blob/main/docs/adr/0017-zero-downtime-deploy-sequence.md)). |
| `deploy` | table | no | Who may deploy this stack through the webhook (§2.5, [ADR-0047](https://github.com/LHelge/hoster/blob/main/docs/adr/0047-oidc-authenticated-deploy-webhook.md)). Without it, the stack can only be deployed from the UI. |

## 2.2 Service

| Field | Type | Required | Rules |
|---|---|---|---|
| `image` | string | yes | Repository reference **without a tag** for CI-built images, e.g. `ghcr.io/acme/notes-api`, whose digest comes from the deploy. Third-party images may use `repo:tag`, `repo:tag@sha256:…` or `repo@sha256:…`. A tag without a digest is **resolved to a digest when the stack is applied**, stored in the stack revision and shown in the apply diff ([ADR-0025](https://github.com/LHelge/hoster/blob/main/docs/adr/0025-registry-credentials-and-image-references.md)). |
| `track` | string | no | Tag the ghcr poller watches for this service, e.g. `"main"` or `"latest"`. Only for CI-built `ghcr.io` images. The poller deploys only when the tag's digest changes ([ADR-0043](https://github.com/LHelge/hoster/blob/main/docs/adr/0043-edge-triggered-poller.md)). |
| `command` | array of string | no | Overrides the image `CMD`. |
| `entrypoint` | array of string | no | Overrides the image `ENTRYPOINT`. |
| `env` | table string → string | no | Plain, non-secret environment variables. For a routed service with exactly one distinct container port, hoster adds `PORT=<that port>` unless `env` (or `secrets`) sets `PORT` itself ([ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md)). A service whose routes use two or more different container ports, or that has no routes, gets no `PORT`; set what it needs in `env`. The injected value is part of the service's config hash, so changing the route's `port` redeploys the service. |
| `secrets` | table string → string | no | Env var name → secret name `<stack>/<name>` in hoster's encrypted secret store ([ADR-0029](https://github.com/LHelge/hoster/blob/main/docs/adr/0029-secret-store-and-config-aware-idempotency.md)). The value never appears in the stack file. Values are injected as environment variables at container create; a deployment that references a secret which is not set is rejected, naming the secret (never its value). Replacing a secret changes its version, which is part of the service's config hash. |
| `volumes` | table string → string | no | Volume name → mount path. Named podman volumes `hoster-<stack>-<volume>` only, kept on stack removal ([ADR-0030](https://github.com/LHelge/hoster/blob/main/docs/adr/0030-named-volumes-and-documented-backups.md)). Host bind mounts are not expressible ([ADR-0019](https://github.com/LHelge/hoster/blob/main/docs/adr/0019-no-container-runtime-access-for-apps.md)). hoster creates each volume on first use, labelled `hoster.managed=true` and `hoster.stack=<stack>`, and refuses to mount an existing one labelled for another stack. Validation warns when a service with volumes uses `update = "start-first"`, since old and new containers would mount the volume at the same time during a deploy ([ADR-0039](https://github.com/LHelge/hoster/blob/main/docs/adr/0039-per-service-update-strategy.md)). |
| `health` | table | no | See §2.4 ([ADR-0038](https://github.com/LHelge/hoster/blob/main/docs/adr/0038-host-driven-health-probes-no-limits-v1.md), [ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md), [ADR-0056](https://github.com/LHelge/hoster/blob/main/docs/adr/0056-service-dependencies-and-exec-health-checks.md)). Defaults: HTTP `GET /healthz` on the first route's container port for routed services, "running for 5 s" for others. Services whose health endpoint is elsewhere (including prefix-mounted ones) declare it; a service without routes may only declare an `exec` check. |
| `depends_on` | array of string | no | Services of this stack that must pass their health check before this one starts, e.g. `["db", "cache"]` ([ADR-0056](https://github.com/LHelge/hoster/blob/main/docs/adr/0056-service-dependencies-and-exec-health-checks.md)). Each must name another service of the stack, at most once; cycles are rejected and named in order (`app → worker → app`). There are no compose-style `condition` variants. Each service starts as soon as its own dependencies pass, during deploys and after a reboot or crash. Outside a deploy, a running dependency that has not passed within min(`start_period`, 5 min) no longer holds its dependents, and a dependency that has never run is ignored. A dependency going down never stops its dependents. Not part of the config hash: changing it reorders starts but never recreates a container. The apply preview lists the start order, and the stack overview shows each service's dependencies and "waiting for db" while the reconciler holds it back. A worked example is in §3.2. |
| `update` | string | no | `"start-first"` (default; zero-downtime) or `"stop-first"` (for services that cannot run twice; short outage with 503 + `Retry-After`) ([ADR-0039](https://github.com/LHelge/hoster/blob/main/docs/adr/0039-per-service-update-strategy.md)). |
| `stop_timeout` | duration | no | Default `10s`. SIGTERM → SIGKILL grace period. |
| `user` | string | no | `uid[:gid]` inside the container. |
| `read_only` | bool | no | Default `false`. Read-only root filesystem. |
| `routes` | array of route | no | Zero or more. If empty, the service publishes no ports ([ADR-0007](https://github.com/LHelge/hoster/blob/main/docs/adr/0007-route-model-and-loopback-only-publishing.md)). |

There are intentionally **no** fields for `ports`, `privileged`, `network_mode`, `cap_add`,
`devices`, bind mounts, or the podman socket ([ADR-0007](https://github.com/LHelge/hoster/blob/main/docs/adr/0007-route-model-and-loopback-only-publishing.md), [ADR-0019](https://github.com/LHelge/hoster/blob/main/docs/adr/0019-no-container-runtime-access-for-apps.md)).

## 2.3 Route

| Field | Type | Required | Default | Rules |
|---|---|---|---|---|
| `host` | string | yes | — | A lowercase FQDN, without port or scheme. No wildcards ([ADR-0016](https://github.com/LHelge/hoster/blob/main/docs/adr/0016-persist-acme-state-defer-dns01.md)). Must resolve to the VPS for ACME. |
| `prefix` | string | no | `"/"` | Must start with `/`. No trailing `/` except the root. No `?`, `#`, `//`, `.` or `..` segments. Normalized into segments for matching and collision checks ([ADR-0009](https://github.com/LHelge/hoster/blob/main/docs/adr/0009-host-and-longest-segment-prefix-routing.md)). |
| `strip_prefix` | bool | no | `false` | Remove the matched prefix before forwarding ([ADR-0010](https://github.com/LHelge/hoster/blob/main/docs/adr/0010-prefix-preservation-and-forwarded-headers.md)). Has no effect on `/`; validation warns if it is set there. |
| `port` | integer | yes | — | Container port, 1–65535, i.e. the port the app listens on inside the container. hoster allocates the host port and binds it on `127.0.0.1`; the app never sees it. **The app must bind `0.0.0.0` or `::`, not `127.0.0.1`:** rootless port forwarding cannot reach a loopback-bound listener. Single-port services also receive it as `PORT` (see `env`). Host-level redirects (`www` → apex) are not expressible in v1 ([ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md)); declare both hosts as routes or redirect in the app. |
| `timeouts` | table | no | hoster defaults | `connect`, `read`, `write`, `idle` as durations ([ADR-0011](https://github.com/LHelge/hoster/blob/main/docs/adr/0011-streaming-bodies-and-per-route-timeouts.md)). |

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
first route's port. Its output tail (the last 256 characters, secret values redacted) is
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

# 4. Versioning

`version = 1` is required. Breaking schema changes bump the version. hoster rejects unknown fields
(`deny_unknown_fields`), so typos fail loudly instead of being ignored. A compatibility policy for
older versions is decided when version 2 exists.
