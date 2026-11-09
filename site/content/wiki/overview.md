---
title: How hoster works
category: Getting Started
---

hoster runs container images that GitHub CI has already built and pushed to ghcr. It runs them as
**stacks** of one or more containers under rootless podman and puts them on the internet behind a
TLS-terminating reverse proxy that it embeds and configures itself. Everything is one binary under
systemd on one VPS.

# Concepts

| Term | Meaning |
|---|---|
| **Stack** | A named unit of deployment: one or more services, one podman network, a set of routes. Defined in a [[stack-format|stack file]]. |
| **Service** | One container within a stack: image, env, volumes, routes, health check. |
| **Route** | `(host, prefix) → service:port`, with `strip_prefix` and timeouts. A (host, prefix) pair is owned by exactly one stack. |
| **Digest** | The immutable `sha256:` content address of an image. hoster deploys digests, never bare tags. |
| **Deployment** | One attempt to bring a stack to a new set of digests or a new configuration, with its history of steps. |

# From a release to running containers

1. **Once per VPS:** [[install]] hoster, give it a GitHub OAuth App and an admin hostname, and
   point DNS for that hostname and every app hostname at the VPS.
2. **Once per app:** in the admin UI, choose **Stacks → New stack** and paste the stack file.
   hoster validates it, shows what will change and applies it. It starts issuing certificates for
   the new hostnames right away. Allow the app's repository to deploy with a `[deploy]` rule.
3. **Every release:** the app's workflow pushes the image and posts its digest to
   `/hooks/deploy` with the job's OIDC token ([[deploying-from-ci]]). hoster then:
   1. pulls `image@digest` and starts the new container next to the old one,
   2. health-checks it on its own loopback port,
   3. swaps the routing table atomically, so new requests go to the new container,
   4. drains in-flight requests to the old container, then removes it.

If a step before the swap fails, the new container is removed and the old one keeps serving. If
the webhook never arrives, the ghcr poller notices the tracked tag's new digest and starts the
same deploy. Both triggers are idempotent: a digest that is already running deploys nothing.

# The admin UI

Every page shares one layout: a sidebar with the version, the hostname and the navigation
(Dashboard, Stacks, Certificates, Settings, System), which moves above the page on a phone. The
colours follow your system's light or dark mode.

| Page | What you see and do |
|---|---|
| **Dashboard** | Request, error-rate, latency, stack and certificate figures; warnings; every stack with its status, healthy services, latency, error shares, restarts and last deploy; the certificates closest to expiry. |
| **Stacks** | Every stack as a card with its services, routes and traffic, filterable by status. **New stack** applies a stack file, with a plan and pre-flight checks before you confirm. |
| **Stack** | One stack under four tabs that share a header with **Redeploy** and **Re-resolve tags**. **Overview**: traffic figures, the slowest routes, the services with their containers, generations, uptime and ports, routes, volumes, deploying explicit digests, applying a new revision of the stack file (with a diff), refused webhook attempts, and removing the stack at the bottom. **Logs**: tail and follow the logs of each service's containers. **Environment**: the environment variables and the secrets, whose values are write-only. **Deployments**: the history, with each step and failure reasons; "Redeploy this one" or "Roll back to this one" is the rollback. |
| **Certificates** | Hosts, issuer, expiry, the last ACME error and the DNS check, per host or for all at once; retry the failing ones. |
| **Settings** | General, registries (credentials for private images), deploy webhook, reverse proxy and certificates. Only registries are edited here; the rest shows the effective `hoster.toml`. |
| **System** | The running and previous versions, the host; check for updates, read the release notes and update. |

There is no separate containers page: a container and its logs are on its stack's pages.

Login is GitHub OAuth, and only the GitHub account named in the configuration gets in. Apps never
get access to podman, and a container is only reachable from outside through a declared route.
