---
title: Running third-party images
category: Guides
---

hoster's home ground is images your own CI builds and deploys by webhook ([[deploying-from-ci]]),
but a stack can run any image from any OCI registry: postgres and redis from Docker Hub, an app
someone else publishes on ghcr or quay, a registry of your own. There is no compose support. A
compose file translates into a stack file by hand, usually in a few minutes, and this guide shows
how. The field-by-field reference is [[stack-format]].

# 1. Image references and registries

## 1.1 Name the registry, and a tag or a digest

A third-party image needs its full reference, registry included. hoster does not guess a
registry for short names: rootless podman resolves them through the host's configuration, so the
same file could pull different images on different hosts.

| Compose writes | The stack file writes |
|---|---|
| `postgres:17` | `docker.io/library/postgres:17` (Docker Hub's official images live under `library/`) |
| `grafana/grafana:11.4.0` | `docker.io/grafana/grafana:11.4.0` |
| `ghcr.io/example/app:2.4` | the same |
| `quay.io/example/app@sha256:…` | the same |

A third-party image needs a tag, a digest or both. Any image that is not on ghcr.io, or that has a
tag or digest, is third-party. Only a bare `ghcr.io/<owner>/<repo>` is treated as built by your
CI, waiting for a webhook or the poller.

## 1.2 Tags are resolved when you apply

A tag is resolved to a digest **when the stack is applied** ([ADR-0025](https://github.com/LHelge/hoster/blob/main/docs/adr/0025-registry-credentials-and-image-references.md)). The apply preview shows
the digest as `pin` or `re-pin` before you confirm, and the revision stores it. Every deploy,
restart and rollback uses that stored digest, so a tag that moves upstream changes nothing
until you ask:

- **Re-resolve tags** on the stack page resolves every tag again, shows which digests moved and
  redeploys only those services once you confirm.
- Applying the file again (for example after changing `postgres:17` to `postgres:17.2`) resolves
  its tags as well.
- `image = "docker.io/library/redis:7@sha256:…"` pins the digest in the file itself, and the tag
  is then only a label. Updating it means editing the file.

Pick the tag for how much should change when you re-resolve: `postgres:17` takes minor
releases, `postgres:17.2` only rebuilds of that release, and `latest` anything, including a new
major version (§4.6).

## 1.3 Registry credentials and Docker Hub's rate limits

Public images need no credentials. Docker Hub limits anonymous pulls per IP address, and a VPS
shares its address with whoever had it before, so pulls can be refused even on a fresh host. A
free Docker account raises the limit:

1. On Docker Hub, create a personal access token with read-only access.
2. In the admin UI, open **Settings → Registries** and add a credential for the host `docker.io`
   with your Docker username and the token. The token is write-only: it is never shown again.
3. Under **Test a registry**, test `docker.io`.

Private images on any registry work the same way: one credential per registry host (`ghcr.io`,
`quay.io`, `registry.example.com`). It serves both resolving tags and podman's pulls.

# 2. Translating a compose file

Go through the compose file service by service. Most keys have a direct counterpart:

| Compose | Stack file |
|---|---|
| `image` | `image`, with its full reference (§1.1). |
| `environment` | `env` for plain values. |
| `environment` with passwords, tokens or keys | `secrets`: `VAR = "<stack>/<name>"`, with the value set on the stack's **Environment** tab. The value never appears in the file. |
| `env_file` | `env` and `secrets`: copy the variables in. There is no file reference. |
| `ports: ["8080:80"]` | A route: `host` is a hostname and `port` the **container** side (`80`). hoster picks the host port and puts it behind HTTPS (§4.1). |
| `expose` | Nothing: services reach each other on the stack network by service name, on any port. |
| `volumes: ["data:/var/lib/app"]` (named) | `volumes = { data = "/var/lib/app" }`: a podman volume `hoster-<stack>-data`, kept when the stack is removed ([[backup]]). |
| `depends_on` | `depends_on`, which always means "has passed its health check", like compose's `condition: service_healthy`. There are no other conditions. |
| `healthcheck.test: ["CMD", …]` | `health.exec`, the same array without `"CMD"`. |
| `healthcheck.test: ["CMD-SHELL", "…"]` or a string | `health.exec = ["sh", "-c", "…"]`, which needs a shell in the image. |
| `healthcheck.interval`, `timeout`, `start_period` | The same names under `health`, as durations like `"30s"`. `retries` has no counterpart: `start_period` bounds how long a deploy waits, and `healthy_after` counts the passes it needs (§4.3). |
| `configs` (a file or content) | `files` with `content`: the text goes into the stack file. |
| `secrets` (under `/run/secrets/`) | `files` with `secret`, at the same path. Or a `secrets` environment variable, if the image reads one. |
| `command`, `entrypoint` | The same, as arrays: split a string into its words. |
| `user`, `read_only` | The same. |
| `stop_grace_period` | `stop_timeout`. |
| `restart` | Nothing: hoster always restarts containers that exit, and starts them after a reboot. |
| `container_name`, `hostname`, `networks`, `links` | Nothing: each stack has one network, and each service's name is its DNS name on it. |
| `shm_size: 256m` | `shm_size = "256MiB"`. Sizes need a unit spelled out: `m` becomes `MiB`, and a bare number of bytes is an error (Stack file format, §2.2.2). |
| `tmpfs: /tmp` or `tmpfs: ["/run:size=16m"]` | `tmpfs = { "/tmp" = "64MiB" }`: every mount needs a size, and gets mode 1777 and no other options. |
| `init: true` | `init = true`. |
| `deploy.resources.limits`, `mem_limit`, `cpus` | Planned for a later release ([ADR-0059](https://github.com/LHelge/hoster/blob/main/docs/adr/0059-runtime-options-and-resource-limits.md)). |

Some compose keys have **no equivalent**, on purpose: an app on hoster gets nothing on the host
beyond its containers ([ADR-0019](https://github.com/LHelge/hoster/blob/main/docs/adr/0019-no-container-runtime-access-for-apps.md)).

| Compose | Why not, and what instead |
|---|---|
| Bind mounts (`./data:/data`, `/var/run/docker.sock:…`) | A stack file can't reach into the host. Data goes in a named volume, configuration in `files`. No container gets the podman socket. |
| `privileged`, `devices`, `cap_add` | Containers keep podman's default rootless privileges. |
| `network_mode: host` | Containers run on their stack's network, and reach the outside only through routes. |
| `build` | hoster runs images, it doesn't build them. Build in CI and push to a registry ([[deploying-from-ci]]). |
| `ports` that aren't HTTP (`5432:5432`, MQTT, SMTP, UDP) | The proxy speaks HTTP and HTTPS only. Services of the same stack reach each other directly; a database is not published to the internet. |

# 3. Worked example: a web app with postgres, redis and a worker

A self-hosted issue tracker, published by its authors as `ghcr.io/example/tracker`, with a
background worker from the same image. Its README offers this compose file:

```yaml
services:
  web:
    image: ghcr.io/example/tracker:2.4
    ports: ["3000:3000"]
    environment:
      DATABASE_URL: postgres://tracker:changeme@db:5432/tracker
      REDIS_URL: redis://cache:6379
      SECRET_KEY: changeme
    configs:
      - source: tracker
        target: /etc/tracker/config.yml
    depends_on:
      db: { condition: service_healthy }
      cache: { condition: service_healthy }
  worker:
    image: ghcr.io/example/tracker:2.4
    command: tracker worker
    environment: # the same as web's
  db:
    image: postgres:17
    environment:
      POSTGRES_USER: tracker
      POSTGRES_PASSWORD: changeme
    volumes: ["pgdata:/var/lib/postgresql/data"]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U tracker"]
  cache:
    image: redis:7
    command: redis-server --appendonly yes
    volumes: ["redis:/data"]
configs:
  tracker:
    file: ./config.yml
volumes: { pgdata: {}, redis: {} }
```

`tracker.stack.toml`:

```toml
#:schema https://docs.hoster.lhelge.se/stack.schema.json
version = 1
name = "tracker"

# ─── The web app: routed, zero-downtime ──────────────────────────────────────
[services.web]
image = "ghcr.io/example/tracker:2.4"          # a tag, so third-party: resolved at apply
depends_on = ["db", "cache"]
env = { REDIS_URL = "redis://cache:6379", TRACKER_LISTEN = "0.0.0.0:3000" }
secrets = { DATABASE_URL = "tracker/database-url", SECRET_KEY = "tracker/secret-key" }

[services.web.files]
"/etc/tracker/config.yml" = { content = """
public_url: https://tracker.example.com
uploads: database
""" }

[[services.web.routes]]
host = "tracker.example.com"
port = 3000

# The image answers its health check on /health, not hoster's default /healthz.
[services.web.health]
http = { path = "/health" }
start_period = "90s"                           # migrations run at start-up

# ─── The worker: same image, another command, no routes ──────────────────────
[services.worker]
image = "ghcr.io/example/tracker:2.4"
command = ["tracker", "worker"]
depends_on = ["db", "cache"]
env = { REDIS_URL = "redis://cache:6379" }
secrets = { DATABASE_URL = "tracker/database-url", SECRET_KEY = "tracker/secret-key" }

# ─── PostgreSQL ──────────────────────────────────────────────────────────────
[services.db]
image = "docker.io/library/postgres:17"
update = "stop-first"                          # one postgres per data directory
stop_timeout = "30s"
env = { POSTGRES_USER = "tracker" }
secrets = { POSTGRES_PASSWORD = "tracker/db-password" }
volumes = { pgdata = "/var/lib/postgresql/data" }
shm_size = "256MiB"                            # parallel queries outgrow the default 62.5 MiB

[services.db.health]
exec = ["pg_isready", "-h", "127.0.0.1", "-U", "tracker"]
start_period = "2m"

# ─── Redis ───────────────────────────────────────────────────────────────────
[services.cache]
image = "docker.io/library/redis:7"
update = "stop-first"                          # one writer per append-only file
command = ["redis-server", "--appendonly", "yes"]
volumes = { redis = "/data" }

[services.cache.health]
exec = ["sh", "-c", "redis-cli ping | grep -q PONG"]
```

**What changed, and why:**

- **Images** got their registry, and Docker Hub's official ones `library/` (§1.1). All four are
  third-party, so their tags resolve to digests when you apply, and the preview lists the pins.
- **`ports`** became a route on a hostname. hoster gets a certificate for it and forwards HTTPS to
  container port 3000. The app's listen address is set to `0.0.0.0` explicitly (§4.1), and because
  `web` has one route port, it also gets `PORT=3000` (§4.2).
- **Passwords** moved to `secrets`. Before the first apply, set them on the stack's Environment
  tab: `tracker/db-password`, `tracker/secret-key`, and `tracker/database-url` as
  `postgres://tracker:<the password>@db:5432/tracker`. The live check in the editor warns about
  each secret that is not set yet.
- **The config file** moved into `files`: the compose file's `./config.yml` would be a bind mount.
  Changing it in the stack file redeploys `web` and nothing else.
- **`depends_on`** lost its conditions: in hoster it always waits for the health check.
- **Health checks.** `web` declares `/health`, because without a `health` table hoster probes
  `/healthz` and the deploy would fail (§4.3). `db`'s `CMD-SHELL` became an `exec` array, with
  `-h 127.0.0.1` so it only passes once postgres listens on TCP rather than during its
  initialisation. `cache` had no healthcheck in compose; it gets one, so `depends_on` can wait for
  redis to finish loading. `worker` has no routes and no `health` table, so it counts as healthy
  once it has run for 5 seconds.
- **`shm_size`** for `db`, which the compose file didn't set: Docker's default `/dev/shm` is 64 MB
  and podman's 62.5 MiB, and Postgres's parallel queries can fail with `could not resize shared
  memory segment` on either.
- **`stop-first`** for `db` and `cache`, which each own a volume (§4.5). `web` keeps the
  zero-downtime default, which is why its `config.yml` says uploads go in the database: a volume
  for uploads would need `stop-first` too.

**Applying it.** The preview's start order reads `1 start cache, db`, then `2 start web, worker`.
After that, editing the file or pressing Re-resolve tags redeploys only what changed: a new redis
digest replaces `cache` stop-first while `web` keeps serving.

# 4. Pitfalls

## 4.1 Listen on `0.0.0.0`, not `127.0.0.1`

hoster forwards each route to a port it binds on the host's loopback. Rootless podman's port
forwarding can't reach a process that listens on the container's own `127.0.0.1`. The container
runs, but the health check never passes and the deploy times out. Images made for compose almost
always listen on all interfaces already. If the logs say `listening on 127.0.0.1:…` or
`localhost:…`, find the image's setting for the bind address (often `HOST`, `BIND`, `LISTEN` or a
flag) and set it to `0.0.0.0`.

## 4.2 The `PORT` variable

A routed service whose routes use exactly one container port gets `PORT=<that port>`, unless its
`env` or `secrets` set `PORT` themselves. Many images read it, and it does no harm to those that
don't. Two things to watch:

- Setting `port` on a route doesn't make the image listen there. If the image ignores `PORT`,
  `port` must be the port the image listens on (its documentation, or `EXPOSE` in its
  Dockerfile).
- An image that uses `PORT` for something else needs `PORT` set in `env` to the value it expects.

## 4.3 Health checks

A routed service without a `health` table is probed with `GET /healthz`. Few third-party images
answer that path. If yours doesn't, the deploy waits for `start_period` and fails. Declare the
image's own health endpoint (`http = { path = "/health" }`), a page that answers without a login
(`path = "/login"`), or `tcp = { port = 3000 }` when nothing better exists.

The image's own `HEALTHCHECK` (in its Dockerfile, or in the compose file) is **never used**:
hoster creates every container with health checks off and runs its own probes ([ADR-0056](https://github.com/LHelge/hoster/blob/main/docs/adr/0056-service-dependencies-and-exec-health-checks.md)).
Translate it to `health.exec` if you want it. The image's Dockerfile shows what it declares, or
on the VPS, once the image is pulled: `podman image inspect <image> | grep -i -A6 healthcheck`.

`start_period` is how long a deploy waits for the first pass. Give a database's first start, which
initialises its data directory, and an app that migrates at start-up a few minutes.

## 4.4 Volumes and user ids

Containers run under rootless podman as the `hoster` user. Root in a container is `hoster` on the
host, and other user ids map to the user's subordinate ids (`/etc/subuid`), so a volume's files
belong to numbers like `100998` on the host. Inside the container everything looks normal.

- **Mount volumes where the image expects its data.** When an empty volume is first mounted,
  podman copies what the image has at the mount path, ownership included. The image's `VOLUME`
  lines or its documentation name the path. A volume mounted elsewhere may end up owned by a user
  the app doesn't run as, and the app fails with "permission denied".
- **Prefer the image's own user.** `user` in the stack file overrides it, and data the old user
  wrote then belongs to someone else. Images that start as root and switch user themselves
  (postgres, redis, nginx) fix their data directory's ownership at start-up; most others don't.
- **Looking at a volume from the host** needs `podman unshare`, which enters the same mapping:

  ```sh
  podman unshare ls -ln "$(podman volume inspect --format '{{.Mountpoint}}' hoster-<stack>-<volume>)"
  ```

  [[backup]] exports and imports volumes with podman, which keeps the ownership.

## 4.5 `stop-first` for anything that holds a volume

By default a deploy starts the new container next to the old one and switches traffic once it
passes. Two copies of a database, a cache that writes an append-only file, or any app that locks
its data directory must not run at the same time on the same volume. Set `update = "stop-first"`
on those services. The apply check warns about a service with volumes that keeps
`start-first` ([ADR-0039](https://github.com/LHelge/hoster/blob/main/docs/adr/0039-per-service-update-strategy.md)). A stop-first service is down for its stop, start and health check;
its routes answer `503` with `Retry-After` meanwhile.

## 4.6 Updating a database to a new major version

A new major version of postgres can't start on an older version's data directory, and many other
databases have the same rule. Re-resolving `postgres:17` stays on 17. Don't move to a new major
with `latest` or by changing the tag in place. Instead:

Do it when nobody uses the app: anything written between the dump and the restore is lost.

1. Dump the database through the running container, as the `hoster` user on the VPS.
   `podman ps --filter label=hoster.stack=tracker` names the container. Keep the dump, and see
   [[backup]] for the rest.

   ```sh
   podman exec <db container> pg_dump -U tracker -Fc tracker > tracker-17.dump
   ```

2. In the stack file, change the tag **and the volume's name** (`pgdata = …` to `pgdata18 = …`),
   and apply. The new version starts on a new, empty volume, and the app may create an empty
   schema in it. The old volume is kept: the stack's danger zone lists it, and applying the old
   file goes back to it.
3. Restore the dump into the new container, replacing whatever the app created:

   ```sh
   podman exec -i <new db container> pg_restore -U tracker -d tracker --clean --if-exists < tracker-17.dump
   ```

4. Check the app. Once it has run on the new version for a while, delete the old volume with
   `podman volume rm hoster-tracker-pgdata`.

Read the image's release notes too: a major version sometimes moves its data directory or
changes its defaults.

# 5. Editing the stack file in the UI

**Stacks → New stack** opens an editor with a starter skeleton. On a stack's page, **Edit stack
file** opens the active revision's file as it was applied, comments included. The editor
highlights the TOML, checks the file while you type and marks problems on their lines. Beside it
are snippets for routes, volumes, health checks, files and tmpfs, and a reference of every field.
**Check** then resolves the tags and shows the plan before you apply. If someone applies another
revision while you edit, the preview says so.

To edit in your own editor instead, keep the `#:schema` line at the top of the file: editors with
TOML schema support then complete and check every field
([[stack-format#1-1-editor-support-the-json-schema|Stack file format, §1.1]]). `hoster stack
check tracker.stack.toml` runs hoster's own checks offline.
