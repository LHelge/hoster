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

Images you didn't build, such as postgres, redis or an app from Docker Hub, run in the same stacks.
Their tags are resolved to digests when you apply the stack file. [[third-party-images]] covers
registries, translating a compose file and the usual pitfalls.

# The admin UI

Every page shares one layout: a sidebar with the version, the hostname, the navigation
(Dashboard, Stacks, Certificates, Settings, System) and the host's resource meters, which moves
above the page on a phone. The colours follow your system's light or dark mode.

| Page | What you see and do |
|---|---|
| **Dashboard** | Traffic over the range you pick (1h, 6h, 24h, 7d or 30d; 24h by default): requests, 5xx rate and p50/p95 latency, each with its change against the period before, and charts of the responses by status and of p50/p95 over time. Stack and certificate figures; warnings; every stack with its status, healthy services, requests per minute, latency, error shares and a sparkline over the range, restarts and last deploy; the certificates closest to expiry; the recent events. The page updates live. |
| **Events** | Everything hoster has recorded in the last 30 days, newest first: deploys and their outcome, container restarts, crash loops and health changes, certificates issued, renewed or failing, and operator and system actions (stacks applied or removed, secrets set or deleted by name, hoster started, updated or rolled back). Filter by kind, severity and stack, including stacks you have removed. Reached from the dashboard's and each stack's Recent events card. |
| **Stacks** | Every stack as a card with its services, routes and last hour of traffic (requests per minute, p95, 5xx share and a sparkline), filterable by status. **New stack** applies a stack file, with a plan and pre-flight checks before you confirm. |
| **Stack** | One stack under four tabs that share a header with **Redeploy** and **Re-resolve tags**. **Overview**: the last hour of traffic (requests per minute and the busiest minute, p50, p95, 5xx rate, the requests in flight now, and the responses by status), the services with their containers, generations, uptime and ports, routes, volumes, deploying explicit digests, applying a new revision of the stack file (with a diff), refused webhook attempts, the stack's recent events, and removing the stack at the bottom. **Logs**: tail and follow the logs of each service's containers. **Environment**: the environment variables and the secrets, whose values are write-only. **Deployments**: the history, with each step and failure reasons; "Redeploy this one" or "Roll back to this one" is the rollback. |
| **Certificates** | Hosts, issuer, expiry, the last ACME error and the DNS check, per host or for all at once; retry the failing ones. |
| **Settings** | General, notifications, registries (credentials for private images), deploy webhook, reverse proxy and certificates. The instance name, [[notifications|mail notifications]] and registries are edited here; the rest shows the effective `hoster.toml`. |
| **System** | The running and previous versions, the host (and the data directory's disk usage when it is on a different filesystem from podman's storage); check for updates, read the release notes and update. |

There is no separate containers page: a container and its logs are on its stack's pages.

## Resource usage

The **HOST** card at the bottom of the sidebar shows the host's CPU (busy share of all cores),
memory (in use, not counting what the kernel can reclaim) and disk (the filesystem podman keeps
its images and volumes on), each with a meter, and CPU and memory with a line of the last hour.
A meter turns orange from 80 % and red from 95 %. hoster samples them every 10 seconds and keeps
an hour in memory only, so the lines start empty after a restart or an update, and a figure it
could not read shows as `—`. The card refreshes itself, but that does not count as activity:
a tab left open still signs you out after the idle timeout.

A stack's overview adds **CPU** and **Memory** columns to its services table, one figure per
container. CPU is a percent of one core (`150%` is one and a half cores), with a line of the
last hour. Memory is what `podman stats` shows: what the container uses, not counting page
cache the kernel would reclaim first. Where the service sets `limits.cpus` or `limits.memory`
([[stack-format#2-2-3-resource-limits-adr-0059|limits]]), the figure is shown against the limit
with a meter in the same colours. A container of another generation (draining or starting
during a deploy) shows its own figures without a meter, and a service with no container shows
`—`. The figures update every 10 seconds while the page is open.

## Traffic history

hoster keeps a history of the traffic to each route: one record per minute for the last 24 hours
and one per hour for the last 60 days, so every range on the dashboard can be compared with the
period before it. It is written to hoster's database once a minute, so the figures survive
restarts and updates, and are up to a minute behind; a crash loses at most the last minute.
Requests per minute is the rate over the last 5 minutes, and only the requests in flight are
live. Routes without requests take no space. The history is part of the database, so
`hoster backup db` copies it with everything else ([[backup]]), and rolling hoster back past the
release that added it removes it.

Login is GitHub OAuth, and only the GitHub account named in the configuration gets in. Apps never
get access to podman, and a container is only reachable from outside through a declared route.
