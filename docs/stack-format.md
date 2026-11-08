# Stack definition format (draft)

Status: draft, 2026-11-07.

Every field follows from an accepted ADR.

## 1. Format choice: TOML

A stack is described in one **TOML** file, conventionally `<stack>.stack.toml`. It lives in the
app's repository and is applied through the web UI (ADR-0027).

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
  people to paste compose files and expect compose semantics (`build:`, `depends_on`, `ports:`,
  `privileged:`). hoster's model is different: routes instead of ports, and a deliberately narrow
  security surface (ADR-0019). A different format makes that obvious.

**Alternatives considered:**

| Format | Why not |
|---|---|
| YAML (compose-like) | Implicit typing pitfalls. The Rust YAML ecosystem has been unsettled since `serde_yaml` was archived (2024). Compose look-alike expectations. |
| JSON | No comments, noisy to write by hand. JSON *is* used on the wire (webhook, internal API). |
| KDL / RON | Nice, but unfamiliar to most people, and editor support is thinner. |
| CUE / Nickel / Dhall | Powerful validation and templating, but a heavy dependency or toolchain for a small schema. |

**Downside accepted:** deeply nested arrays of tables (`[[services.api.routes]]`) are verbose.
Inline tables are allowed where they read better.

## 2. Schema

### 2.1 Top level

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
| `drain_timeout` | duration | no | Default `30s`. Upper bound for draining old upstreams (ADR-0017). |
| `deploy` | table | no | Who may deploy this stack through the webhook (§2.5, ADR-0047). Without it, the stack can only be deployed from the UI. |

### 2.2 Service

| Field | Type | Required | Rules |
|---|---|---|---|
| `image` | string | yes | Repository reference **without a tag** for CI-built images, e.g. `ghcr.io/acme/notes-api`, whose digest comes from the deploy. Third-party images may use `repo:tag`, `repo:tag@sha256:…` or `repo@sha256:…`. A tag without a digest is **resolved to a digest when the stack is applied**, stored in the stack revision and shown in the apply diff (ADR-0025). |
| `track` | string | no | Tag the ghcr poller watches for this service, e.g. `"main"` or `"latest"`. Only for CI-built `ghcr.io` images. The poller deploys only when the tag's digest changes (ADR-0043). |
| `command` | array of string | no | Overrides the image `CMD`. |
| `entrypoint` | array of string | no | Overrides the image `ENTRYPOINT`. |
| `env` | table string → string | no | Plain, non-secret environment variables. For a routed service with exactly one distinct container port, hoster adds `PORT=<that port>` unless `env` (or `secrets`) sets `PORT` itself (ADR-0051). A service whose routes use two or more different container ports, or that has no routes, gets no `PORT`; set what it needs in `env`. The injected value is part of the service's config hash, so changing the route's `port` redeploys the service. |
| `secrets` | table string → string | no | Env var name → secret name `<stack>/<name>` in hoster's encrypted secret store (ADR-0029). The value never appears in the stack file. Values are injected as environment variables at container create; a deployment that references a secret which is not set is rejected, naming the secret (never its value). Replacing a secret changes its version, which is part of the service's config hash. |
| `volumes` | table string → string | no | Volume name → mount path. Named podman volumes `hoster-<stack>-<volume>` only, kept on stack removal (ADR-0030). Host bind mounts are not expressible (ADR-0019). hoster creates each volume on first use, labelled `hoster.managed=true` and `hoster.stack=<stack>`, and refuses to mount an existing one labelled for another stack. Validation warns when a service with volumes uses `update = "start-first"`, since old and new containers would mount the volume at the same time during a deploy (ADR-0039). |
| `health` | table | no | See §2.4 (ADR-0038, ADR-0051). Defaults: HTTP `GET /healthz` on the first route's container port for routed services, "running for 5 s" for others. Services whose health endpoint is elsewhere (including prefix-mounted ones) declare it. |
| `update` | string | no | `"start-first"` (default; zero-downtime) or `"stop-first"` (for services that cannot run twice; short outage with 503 + `Retry-After`) (ADR-0039). |
| `stop_timeout` | duration | no | Default `10s`. SIGTERM → SIGKILL grace period. |
| `user` | string | no | `uid[:gid]` inside the container. |
| `read_only` | bool | no | Default `false`. Read-only root filesystem. |
| `routes` | array of route | no | Zero or more. If empty, the service publishes no ports (ADR-0007). |

There are intentionally **no** fields for `ports`, `privileged`, `network_mode`, `cap_add`,
`devices`, bind mounts, or the podman socket (ADR-0007, ADR-0019).

### 2.3 Route

| Field | Type | Required | Default | Rules |
|---|---|---|---|---|
| `host` | string | yes | — | A lowercase FQDN, without port or scheme. No wildcards (ADR-0016). Must resolve to the VPS for ACME. |
| `prefix` | string | no | `"/"` | Must start with `/`. No trailing `/` except the root. No `?`, `#`, `//`, `.` or `..` segments. Normalized into segments for matching and collision checks (ADR-0009). |
| `strip_prefix` | bool | no | `false` | Remove the matched prefix before forwarding (ADR-0010). Has no effect on `/`; validation warns if it is set there. |
| `port` | integer | yes | — | Container port, 1–65535, i.e. the port the app listens on inside the container. hoster allocates the host port and binds it on `127.0.0.1`; the app never sees it. **The app must bind `0.0.0.0` or `::`, not `127.0.0.1`:** rootless port forwarding cannot reach a loopback-bound listener. Single-port services also receive it as `PORT` (see `env`). Host-level redirects (`www` → apex) are not expressible in v1 (ADR-0051); declare both hosts as routes or redirect in the app. |
| `timeouts` | table | no | hoster defaults | `connect`, `read`, `write`, `idle` as durations (ADR-0011). |

**Validation (a deploy or apply fails as a whole):**

- Duplicate (host, prefix) inside the stack → error.
- (host, prefix) owned by another stack → error (ADR-0012).
- More than one service of the same stack on the same (host, prefix) → error, same as a duplicate.
- Durations parse as humantime strings: `"500ms"`, `"30s"`, `"5m"`.

### 2.4 Health check (ADR-0038)

```toml
[services.api.health]
http = { path = "/healthz", port = 8080, expect_status = "2xx" }  # or: tcp = { port = 8080 }
interval = "1s"
timeout = "2s"
healthy_after = 2     # consecutive successes required
start_period = "30s"  # give up and abort the deploy after this
```

hoster runs the probe from the host against the container's allocated `127.0.0.1` port. Without a
`health` table, a routed service is probed with `GET /healthz` on its first route's container port,
expecting 2xx–3xx (ADR-0051); set `health.http.path` when the endpoint differs, e.g. `/api/healthz`
for a service mounted under a preserved prefix. Services without routes have no loopback port. For
them v1 uses "running and not restarted for 5 s", and `health` is not allowed on non-routed
services.

Unset timings default to `interval = "1s"`, `timeout = "2s"`, `healthy_after = 1` and
`start_period = "30s"`. `path` is sent as written, so spaces, control and non-ASCII characters
must be percent-encoded. HTTP probes do not follow redirects, so a 3xx answer is the result. A
deploy aborts as soon as the container exits during the check. Probe results (each change, and the
verdict) are recorded as deployment events.

### 2.5 Deploy authorization (ADR-0047)

```toml
[deploy]
repository_id = 123456789                # required: GitHub's numeric repo id (stable across renames)
refs = ["refs/tags/v*"]                  # optional: glob patterns matched against the token's `ref`
environment = "production"               # optional: must equal the token's `environment`
job_workflow_ref = "acme/notes/.github/workflows/release.yml@refs/heads/main"  # optional
```

| Field | Type | Required | Rules |
|---|---|---|---|
| `repository_id` | integer | yes | Compared with the OIDC token's `repository_id` claim. The stack page shows the claims of rejected attempts, which helps find it. |
| `refs` | array of string | no | Globs: `*` matches within one path segment, `**` across segments. At least one must match `ref`. |
| `environment` | string | no | Exact match on the `environment` claim. Pair it with GitHub environment protection rules (required reviewers, tag-only deployment). |
| `job_workflow_ref` | string | no | Exact match. Use it to require a specific reusable or release workflow. |

A webhook may only deploy services whose `image` is in a repository the token's repo publishes
to. The digest-ownership check from ADR-0042 still applies.

## 3. Worked example: Rust API at `/api`, nginx frontend at `/`, rustfs on `s3.`

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

### 3.1 What hoster does with it

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
| `GET https://notes.example.com/apiary` | web (segment boundary, ADR-0009) | `/apiary` |
| `PUT https://s3.notes.example.com/attachments/abc.png` | storage | `/attachments/abc.png`, `Host: s3.notes.example.com`, signature intact |

**Certificates:** one each for `notes.example.com` and `s3.notes.example.com`, through HTTP-01.

**Deploys:**

- A webhook carrying a new `api` digest replaces only `api` (start-first). `web` and `storage`
  are untouched, because their digests are unchanged.
- Applying a revision whose `storage` tag resolves to a new digest (or pressing "re-resolve
  tags") redeploys `storage` stop-first, which causes a short storage outage. The file says so
  explicitly (ADR-0039).

## 4. Versioning

`version = 1` is required. Breaking schema changes bump the version. hoster rejects unknown fields
(`deny_unknown_fields`), so typos fail loudly instead of being ignored. A compatibility policy for
older versions is decided when version 2 exists.
