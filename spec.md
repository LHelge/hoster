# hoster — specification

Status: draft, 2026-11-07. Planning only; no implementation exists yet.

## 1. Purpose

hoster is a self-hosted deployment orchestrator for a single VPS, in the spirit of Coolify but
much smaller. It takes container images that GitHub CI has already built and pushed to ghcr,
runs them as **stacks** of one or more containers under rootless podman, and puts them on the
internet behind a TLS-terminating reverse proxy that it embeds and configures itself.

The goal is that shipping an app is just "push a release" on GitHub. A minute later the new
version is serving traffic on its hostname with a valid certificate, and no request was dropped
during the switch.

hoster is one self-contained Rust binary. It runs on the host under systemd, talks to the rootless
podman socket, and includes:

- the reverse proxy (Pingora)
- certificate management (ACME)
- the deploy engine
- the webhook receiver and the registry poller
- a web admin interface, built with Axum, Askama and vendored HTMX 4, with GitHub OAuth login
  (ADR-0021)

## 2. Scope

In scope:

- **Deploying stacks.** A stack is a named group of services. Each service is one container image
  pinned by digest. Each stack gets its own podman network, and its services reach each other by
  service name.
- **Routing.** A service declares zero or more routes. A route is a host, an optional path prefix,
  a `strip_prefix` flag, a container port and optional timeouts. Routing matches on the host and
  then the longest path prefix, on segment boundaries.
- **Exposure.** Only routed services publish ports, and only on `127.0.0.1`, on ports hoster
  allocates. Everything public goes through hoster's proxy on 80/443.
- **TLS.** Certificates are issued and renewed automatically through ACME HTTP-01 (Let's Encrypt
  or any RFC 8555 CA). Port 80 serves ACME challenges and redirects everything else to HTTPS.
- **Deploy triggers.**
  - The main trigger is a webhook sent by the app's release workflow, authenticated with the
    job's GitHub Actions OIDC token (ADR-0047). It carries the
    image digest.
  - The fallback is polling ghcr for tag → digest changes.
  - Both triggers are idempotent by digest: if the digest is already running, nothing happens.
- **Zero-downtime deploys.** hoster starts the new container, health-checks it, swaps the upstream
  atomically, drains the old container and then removes it.
- **Deploy validation.** A deploy is rejected if one of its routes collides with a route that
  another stack already owns, i.e. the same (host, prefix).
- **Web admin interface.**
  - Pages: stacks, deployments, health, certificates and logs.
  - Actions: apply stacks, deploy, roll back.
  - Built with Axum, Askama templates and vendored HTMX 4.
  - Login is GitHub OAuth. Only the GitHub account named in `HOSTER_ADMIN_GITHUB` gets in
    (ADR-0021).

## 3. Non-goals

- **App-specific features.** hoster has no built-in databases, buildpacks, "one-click" templates,
  or knowledge of particular frameworks. Everything is an image plus a stack definition.
- **Container-runtime access for apps.** Apps never get the podman socket and never get nested
  containers. A compromised app must not be able to control the host's containers.
- **Multiple hosts.** No clustering, scheduling, overlay networks or replication. One VPS.
- **Building images.** CI builds images. hoster only pulls them.
- **Wildcard certificates and DNS-01** are deferred until wildcards are actually needed.
- **A GitHub App integration** is deferred. Webhook plus polling cover the need for now.

## 4. Concepts

| Term | Meaning |
|---|---|
| **Stack** | A named unit of deployment: one or more services, one podman network, a set of routes. Defined in a stack file (see [site/content/wiki/stack-format.md](site/content/wiki/stack-format.md)). |
| **Service** | One container within a stack: image, env, volumes, routes, health check. |
| **Route** | `(host, prefix) → service:port`, with `strip_prefix` and timeouts. A (host, prefix) pair is owned by exactly one stack. |
| **Digest** | The immutable `sha256:` content address of an image (manifest or index). hoster deploys digests, never bare tags. |
| **Generation** | A monotonically increasing number per stack, assigned to each deploy attempt and recorded per service: only services whose image or config changed move to the new generation, the others keep theirs. Container names and labels carry it, so the old and new containers can run side by side. |
| **Upstream** | A `127.0.0.1:<allocated port>` endpoint that the proxy forwards to for one container of one routed service. |

## 5. Workflow: from CI push to running app

This is the user-facing flow. The decisions behind each step are recorded in [docs/adr/](docs/adr/).

### 5.1 One-time setup on the VPS

1. Run the install script as root:
   `curl -fsSL https://github.com/LHelge/hoster/releases/latest/download/install.sh | sudo bash`.
   It creates a `hoster` user and enables linger for it, enables its user-level `podman.socket`,
   sets `net.ipv4.ip_unprivileged_port_start=80`, and installs the binary under
   `~hoster/.local/lib/hoster` plus the user unit `hoster.service` (ADR-0023, ADR-0052). It
   never starts the service; see [site/content/wiki/install.md](site/content/wiki/install.md).
2. Edit `~/.config/hoster/hoster.toml`, which the script created from the example: ACME contact
   email and directory, admin hostname (e.g. `hoster.example.com`) and upstream port range.
   Secrets go in separate files (ADR-0024).
3. Create a GitHub OAuth App with callback `https://hoster.example.com/auth/github/callback`.
   Give hoster its client id/secret, and set `HOSTER_ADMIN_GITHUB=<your handle>` in the unit's
   environment (ADR-0021, ADR-0031).
4. Point DNS (A/AAAA records) for the admin hostname and every app hostname at the VPS.
5. Start the service. hoster binds :80 and :443, loads any certificates it already has, and
   reconciles the containers it knows about. It obtains a certificate for the admin hostname, after
   which you can sign in at `https://hoster.example.com` with GitHub.

### 5.2 One-time setup per app

1. Write a stack file, e.g. `notes.stack.toml`, describing services, routes and health checks.
2. In the web UI, choose **Stacks → New stack** and upload or paste the file. hoster validates
   it (schema, route collisions), shows what will change and which services will be redeployed,
   and applies it on confirmation, redeploying those services (ADR-0027, ADR-0049). It starts
   issuing certificates for the new hostnames right away (ADR-0036). An advisory DNS check
   compares each host's records with hoster's own addresses and flags mismatches in the UI as the
   likely cause of a failing issuance (ADR-0051).
3. If images are private, enter a classic PAT with `read:packages` under **Settings → Registries**
   (ADR-0025). Enter the stack's secrets on its **environment** tab (ADR-0029). Values are
   encrypted at rest and write-only in the UI.
4. Allow the app's repository to deploy the stack by adding a `[deploy]` section to the stack
   file with the repository's numeric id, plus optional ref/environment restrictions. If you don't
   know the id, the first rejected deploy attempt shows it on the stack page (ADR-0047). No
   secret is stored in CI.
5. Add a deploy step to the app's GitHub release workflow. It builds and pushes the image, takes
   the digest from the push step, requests an OIDC token and sends the webhook:

   ```yaml
   permissions:
     contents: read
     packages: write
     id-token: write        # lets the job request an OIDC token for hoster
   # …
   - name: Notify hoster
     env:
       HOSTER_URL: https://hoster.example.com/hooks/deploy
       HOSTER_AUDIENCE: https://hoster.example.com
     run: |
       token=$(curl -fsS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
         "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=$HOSTER_AUDIENCE" | jq -r .value)
       body=$(jq -nc --arg d "${{ steps.push.outputs.digest }}" --arg t "${{ github.ref_name }}" \
         '{stack:"notes", images:{api:$d}, tag:$t}')
       curl -fsS -X POST "$HOSTER_URL" \
         -H 'Content-Type: application/json' \
         -H "Authorization: Bearer $token" \
         --data "$body"
   ```

   hoster verifies the token (GitHub's signature, issuer, audience, expiry, single-use `jti`) and
   checks its claims against the stack's `[deploy]` rule. Several services may be listed in
   `images` to deploy them together (ADR-0042, ADR-0047). The full guide, with a complete
   workflow to copy, is [site/content/wiki/deploying-from-ci.md](site/content/wiki/deploying-from-ci.md).

### 5.3 Every release

1. A release is published (tag pushed) on GitHub.
2. CI builds the image and pushes `ghcr.io/acme/notes-api:v1.4.2`. The push step outputs digest
   `sha256:ab12…`.
3. CI POSTs the webhook `{stack: "notes", images: {api: "sha256:ab12…"}}` with its OIDC token.
4. hoster verifies the token and the stack's `[deploy]` rule, then checks idempotency. If `api` is already running
   `sha256:ab12…`, it answers `200 {"result":"noop"}` and stops here.
5. Otherwise it answers `202 Accepted` with a deployment id and starts a deploy:
   1. **Validate.** Check the stack file and route collisions, and allocate a `127.0.0.1` port
      for each routed service that changes.
   2. **Pull** `ghcr.io/acme/notes-api@sha256:ab12…`.
   3. **Start** the new container, generation *n+1*, on the stack's network next to the old one.
   4. **Health-check** it on its allocated port until it passes or times out.
   5. **Swap.** Publish a new routing table in which `/api` on `notes.example.com` points at the
      new upstream. New requests go to the new container from this instant on.
   6. **Drain.** Wait until no request is in flight to the old upstream, or until the drain
      timeout expires.
   7. **Clean up.** Stop and remove the old container and release its port.
6. If a step before the swap fails, the new container is removed and the old one keeps serving.
   The deployment is recorded as failed, with the reason.
7. If the webhook never arrives (CI hiccup, network partition), the ghcr poller notices that the
   tracked tag now points at a new digest and starts the same deploy. It only reacts when a tracked tag's digest changes (ADR-0043).
8. The first time a host appears, hoster obtains a certificate for it over HTTP-01 on port 80.
   Issuance already starts when the stack is applied, and deploys never wait for it: until the
   certificate exists, HTTPS for that host fails and the UI shows "TLS pending" (ADR-0036).

### 5.4 Day-to-day operations in the web UI

| Page | What you see and do |
|---|---|
| **Dashboard** | Every stack and service: running digest, health, last deploy outcome, certificate expiry. Live updates through HTMX. |
| **Stack** | Services, routes, generations. Apply a new revision of the stack file (with diff), manage secrets, see the `[deploy]` rule and recently rejected webhook attempts with their token claims, redeploy, remove the stack (volumes kept unless explicitly deleted). |
| **Deployments** | History per stack, with state transitions and failure reasons. "Redeploy this one" acts as rollback (ADR-0041). |
| **Logs** | Tail and follow logs per container. |
| **Certificates** | Hosts, issuer, expiry, last ACME error, DNS check. |
| **Settings** | Registry credentials. |
| **System** | Running version, target and schema; installed current and previous versions. "Check for updates" asks GitHub for the latest stable release only when clicked and shows its notes; **Update** installs it and restarts hoster. Rollback is `hoster rollback` over SSH (ADR-0052). |

There is no break-glass login: if GitHub login is unavailable, use SSH until it is back
(ADR-0031). Running apps and the webhook are unaffected.

## 6. Behavioural guarantees

- **Idempotent by digest and config.** Delivering the same webhook any number of times, or having
  the poller and the webhook both fire, gives exactly one deploy per (stack, digest set). A service
  redeploys only if its image digest or its config hash (env, secret versions, volumes, routes…)
  changed. Config changes redeploy the affected services when you confirm the apply; there is no
  staged apply, so outside a failed deployment every service runs the active revision (ADR-0029,
  ADR-0049).
- **No partial routing state.** The proxy sees either the old routing table or the new one, never
  a mix. Each request is routed against one consistent snapshot.
- **Route ownership.** A (host, prefix) pair belongs to at most one stack. A deploy that would
  take over another stack's route is rejected before anything starts.
- **Prefix semantics.**
  - The path prefix is forwarded unchanged by default. With `strip_prefix = true` it is removed
    before forwarding.
  - `X-Forwarded-Prefix` is set either way.
  - The `Host` header is forwarded exactly as the client sent it.
- **Streaming.** Request and response bodies are streamed in both directions. Neither is
  buffered in full, so large uploads, downloads and server-sent events work.
- **Failure containment.** A failed deploy never leaves a stack without a serving container if it
  had one before.
- **Isolation.** Each stack has its own network. A container is only reachable from outside
  through a declared route.

## 7. Related documents

- [docs/architecture.md](docs/architecture.md): components, threads, channels, state machine.
- [site/content/wiki/stack-format.md](site/content/wiki/stack-format.md): the stack definition format.
- [docs/adr/](docs/adr/): one record per architectural decision.
- [docs/open-questions.md](docs/open-questions.md): undecided items, each with a recommendation.
