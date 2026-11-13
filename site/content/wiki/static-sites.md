---
title: Hosting a static site
category: Guides
---

hoster has no special type for static sites ([ADR-0018](https://github.com/LHelge/hoster/blob/main/docs/adr/0018-no-app-specific-features.md)). A site built by a static site generator
is packed into a small image with a static-file server in CI and deployed like any other service
([ADR-0003](https://github.com/LHelge/hoster/blob/main/docs/adr/0003-images-built-in-ci-pushed-to-ghcr.md)). It gets everything other services get: private repositories and packages, deploys by
digest, zero-downtime swaps, rollback to earlier images and logs in the UI.

The example uses [aphid-server](https://aphid.lhelge.se/wiki/aphid-server/), a static-file server
made for sites built with aphid and run behind a proxy like hoster's. Any static server works:
`nginx-unprivileged` with a `default.conf` that listens on 8080 and answers `/healthz` is the
usual alternative.

This documentation is deployed this way: its source, `Dockerfile` and stack file are in
[`site/`](https://github.com/LHelge/hoster/tree/main/site), and the `site` and `docs` jobs of
[`release.yml`](https://github.com/LHelge/hoster/blob/main/.github/workflows/release.yml) build
and deploy it on every release.

Files to copy into the site's repository are in
[`examples/static-site/`](https://github.com/LHelge/hoster/tree/main/examples/static-site):

| File | What it does |
|---|---|
| [`Dockerfile`](https://github.com/LHelge/hoster/blob/main/examples/static-site/Dockerfile) | Two lines: `FROM ghcr.io/lhelge/aphid-server`, then copies the built site from `dist/` to `/site`. The server runs as non-root, answers `/healthz` and serves `404.html` for missing pages. |
| [`release.yml`](https://github.com/LHelge/hoster/blob/main/examples/static-site/release.yml) | On every push to `main`: builds the site with [aphid](https://github.com/LHelge/aphid), builds and pushes the image, deploys the digest with hoster's deploy action. |

# 1. The stack file

```toml
version = 1
name = "blog"

[services.web]
image = "ghcr.io/acme/blog"        # pushed by release.yml, no tag

[services.web.env]
APHID_LOG_FORMAT = "json"          # access logs with levels and fields in the UI

[[services.web.routes]]
host = "blog.example.com"
port = 8080

[deploy]
repository_id = 123456789          # gh api repos/acme/blog --jq .id
refs = ["refs/heads/main"]
environment = "production"
```

No `health` table is needed: the default probe is `GET /healthz` on the route's port
([ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md)), and aphid-server answers it. hoster also injects `PORT` set to the route's port,
and aphid-server listens on it, so the route's `port` is the only place the port is named. (A
server that ignores `PORT`, such as nginx, must listen on the route's port itself.)

For a private package, enter a PAT with `read:packages` under **Settings → Registries**
([ADR-0025](https://github.com/LHelge/hoster/blob/main/docs/adr/0025-registry-credentials-and-image-references.md)). Setting up the `[deploy]` rule and the `production` environment is the same as for
any app: see [[deploying-from-ci#1-one-time-setup|Deploying from CI, §1]]. For a site that deploys from `main`
rather than from tags, limit the environment to the `main` branch.

# 2. The workflow

`release.yml` is [`examples/github-actions/release.yml`](https://github.com/LHelge/hoster/blob/main/examples/github-actions/release.yml)
with two changes: it runs on pushes to `main` instead of `v*` tags, and it builds the site before
building the image. The deploy step is the same `LHelge/hoster/actions/deploy`
([[deploying-from-ci#2-3-deploy-with-hoster-s-action|Deploying from CI, §2.3]]); off a tag, the
deploy history shows the short commit SHA. The build step is aphid's GitHub Action:

```yaml
- uses: LHelge/aphid@v0.5.0
  with:
    version: v0.5.0     # the aphid binary; defaults to the latest release
    config: aphid.toml
    output: dist
    precompress: true   # also write .br and .gz siblings for aphid-server
```

Any other generator works the same way, as long as its output ends up in `dist/` before
`docker/build-push-action` runs. Building outside the Dockerfile keeps the image to the server
plus the site, and lets the generator use the runner's caches. Keep the `aphid-server` tag in the
Dockerfile at the same version as the action.

## Without the webhook

If deploying within a few minutes is good enough, the deploy step can be dropped. Add
`track = "main"` to the service, and the ghcr poller deploys whenever the `main` tag that
`docker/metadata-action` pushes gets a new digest ([ADR-0043](https://github.com/LHelge/hoster/blob/main/docs/adr/0043-edge-triggered-poller.md); every 5 minutes by default,
`[poller] interval` in `hoster.toml`). The workflow then needs neither `id-token: write` nor a
`[deploy]` rule. The webhook is still the better choice when you want the deploy in the run's
log and a failed deploy to fail the run.

# 3. Things to know

- **Redirects:** aphid-server redirects a directory without a trailing slash (`/posts` →
  `/posts/`) with a relative `Location`, so it stays on `https://` and the public host instead of
  pointing at `http://<host>:8080`. With nginx, set `absolute_redirect off` for the same result.
- **Clean URLs:** aphid writes every page as `<page>/index.html`, so `/wiki/install/` needs no
  rewrite. aphid-server does not map `/page` to `page.html`; a generator that writes `page.html`
  files needs a server that does (nginx's `try_files $uri $uri/ $uri.html`).
- **`www` → apex:** hoster has no host redirects in v1 ([ADR-0051](https://github.com/LHelge/hoster/blob/main/docs/adr/0051-poller-bootstrap-advisory-dns-health-default-port.md)), and aphid-server has none
  either. Route both hosts to the service, or run a second small service for `www` that only
  redirects (an nginx `server` block returning `301 https://example.com$request_uri`) and route
  `www.example.com` to it.
- **Compression:** hoster's proxy does not compress, so the server must. aphid-server sends text
  responses with brotli or gzip, as the client's `Accept-Encoding` prefers. With
  `precompress: true` the build writes `.br` and `.gz` siblings, which are sent as they are.
  Anything without a sibling is compressed on every request. With nginx, set `gzip on`.
- **Under a prefix:** the prefix is preserved by default ([ADR-0010](https://github.com/LHelge/hoster/blob/main/docs/adr/0010-prefix-preservation-and-forwarded-headers.md)), so a site routed at
  `/docs` must be built with that base path (aphid's, Hugo's or Zola's base URL setting).
  Alternatively, set `strip_prefix = true` on the route, but links in the site must then still
  include the prefix.
- **Caching:** aphid-server sets no `Cache-Control`, so browsers revalidate with
  `Last-Modified`/`ETag` and get `304 Not Modified` for unchanged files.
- **Architecture:** build for the VPS's architecture (`platforms:` in the workflow). The
  aphid-server image is multi-arch.
