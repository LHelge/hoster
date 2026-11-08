---
title: Hosting a static site
category: Guides
---

hoster has no special type for static sites ([ADR-0018](https://github.com/LHelge/hoster/blob/main/docs/adr/0018-no-app-specific-features.md)). A site built by a static site generator
is packed into a small nginx image in CI and deployed like any other service ([ADR-0003](https://github.com/LHelge/hoster/blob/main/docs/adr/0003-images-built-in-ci-pushed-to-ghcr.md)). It gets
everything other services get: private repositories and packages, deploys by digest,
zero-downtime swaps, rollback to earlier images and logs in the UI. An idle nginx container uses
a few megabytes of memory.

This documentation is deployed this way: its source, `Dockerfile` and stack file are in
[`site/`](https://github.com/LHelge/hoster/tree/main/site), and the `site` and `docs` jobs of
[`release.yml`](https://github.com/LHelge/hoster/blob/main/.github/workflows/release.yml) build
and deploy it on every release.

Files to copy into the site's repository are in
[`examples/static-site/`](https://github.com/LHelge/hoster/tree/main/examples/static-site):

| File | What it does |
|---|---|
| [`Dockerfile`](https://github.com/LHelge/hoster/blob/main/examples/static-site/Dockerfile) | Copies the built site from `dist/` into `nginx-unprivileged`, which runs as non-root and listens on 8080. |
| [`default.conf`](https://github.com/LHelge/hoster/blob/main/examples/static-site/default.conf) | Serves the site, answers `/healthz`, serves `404.html` for missing pages and turns off absolute redirects. |
| [`release.yml`](https://github.com/LHelge/hoster/blob/main/examples/static-site/release.yml) | On every push to `main`: builds the site with [aphid](https://github.com/LHelge/aphid), builds and pushes the image, posts the digest to hoster. |

# 1. The stack file

```toml
version = 1
name = "blog"

[services.web]
image = "ghcr.io/acme/blog"        # pushed by release.yml, no tag

[[services.web.routes]]
host = "blog.example.com"
port = 8080                        # the container port nginx listens on

[deploy]
repository_id = 123456789          # gh api repos/acme/blog --jq .id
refs = ["refs/heads/main"]
environment = "production"
```

No `health` table is needed: the default probe is `GET /healthz` on the route's port
([ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md)), and `default.conf` answers it. hoster also injects `PORT=8080`, which nginx ignores;
the `listen` line in `default.conf` and the route's `port` must agree.

For a private package, enter a PAT with `read:packages` under **Settings → Registries**
([ADR-0025](https://github.com/LHelge/hoster/blob/main/docs/adr/0025-registry-credentials-and-image-references.md)). Setting up the `[deploy]` rule and the `production` environment is the same as for
any app: see [[deploying-from-ci#1-one-time-setup|Deploying from CI, §1]]. For a site that deploys from `main`
rather than from tags, limit the environment to the `main` branch.

# 2. The workflow

`release.yml` is [`examples/github-actions/release.yml`](https://github.com/LHelge/hoster/blob/main/examples/github-actions/release.yml)
with two changes: it runs on pushes to `main` instead of `v*` tags, and it builds the site before
building the image. The build step is aphid's GitHub Action:

```yaml
- uses: LHelge/aphid@v0.3.0
  with:
    version: v0.3.0     # the aphid binary; defaults to the latest release
    config: aphid.toml
    output: dist
```

Any other generator works the same way, as long as its output ends up in `dist/` before
`docker/build-push-action` runs. Building outside the Dockerfile keeps the image to nginx plus
the site, and lets the generator use the runner's caches.

## Without the webhook

If deploying within a few minutes is good enough, the deploy step can be dropped. Add
`track = "main"` to the service, and the ghcr poller deploys whenever the `main` tag that
`docker/metadata-action` pushes gets a new digest ([ADR-0043](https://github.com/LHelge/hoster/blob/main/docs/adr/0043-edge-triggered-poller.md); every 5 minutes by default,
`[poller] interval` in `hoster.toml`). The workflow then needs neither `id-token: write` nor a
`[deploy]` rule. The webhook is still the better choice when you want the deploy in the run's
log and a failed deploy to fail the run.

# 3. Things to know

- **Redirects:** nginx redirects a directory without a trailing slash (`/posts` → `/posts/`).
  With `absolute_redirect off` the `Location` is relative, so it stays on `https://` and the
  public host instead of pointing at `http://<host>:8080`.
- **`www` → apex:** hoster has no host redirects in v1 ([ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md)). Either route both hosts to
  the service, or add a `server` block for `www` in `default.conf` that returns
  `301 https://example.com$request_uri` and route `www.example.com` to the same port.
- **Under a prefix:** the prefix is preserved by default ([ADR-0010](https://github.com/LHelge/hoster/blob/main/docs/adr/0010-prefix-preservation-and-forwarded-headers.md)), so a site routed at
  `/docs` must be built with that base path (aphid's, Hugo's or Zola's base URL setting).
  Alternatively, set `strip_prefix = true` on the route, but links in the site must then still
  include the prefix.
- **Caching:** `default.conf` sets no `Cache-Control`, so browsers revalidate with
  `Last-Modified`/`ETag`. If the generator fingerprints asset file names, a
  `location ~* \.(css|js|woff2)$ { expires 1y; }` block is safe to add.
- **Architecture:** build for the VPS's architecture (`platforms:` in the workflow). The nginx
  base image is multi-arch.
