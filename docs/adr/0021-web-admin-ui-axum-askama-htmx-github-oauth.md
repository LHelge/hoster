# ADR-0021: Web admin interface: Axum + Askama + vendored HTMX 4, GitHub OAuth login, admin handle from env

- Status: Accepted
- Date: 2026-11-07

## Context

hoster needs an operator interface for:

- viewing stacks, deployments, health, certificates and logs
- triggering deploys and rollbacks
- managing stacks

The operator is one person who already has a GitHub identity, because the apps, CI and ghcr all
live on GitHub.

## Decision

- **Server.** The admin interface is a **server-rendered web UI** served by **Axum** (axum 0.8.9
  checked) in the orchestrator's tokio runtime.
- **Templates.** HTML is rendered with **Askama** (0.16.1 checked). Templates are checked and
  compiled into the binary.
- **Interactivity** comes from **HTMX 4** (htmx.org 4.0.1, released 2026-11-01, checked on npm),
  **vendored** into the repository and embedded in the binary as a static asset. Nothing is
  loaded from a CDN.
- **Login** is **GitHub OAuth** (authorization code flow).
- **Authorization.** Exactly one admin GitHub handle is given in an **environment variable**.
  Only that GitHub account can use the UI.

## Alternatives considered

The project owner made this decision. These alternatives are recorded for context:

- **CLI over a Unix socket**: simplest auth (file permissions), but needs shell access to the VPS
  for every operation.
- **JSON API + SPA**: a separate frontend build and toolchain, and more surface. HTMX keeps all
  logic server-side and needs no JS build.
- **Local accounts / passwords / passkeys**: hoster would have to manage credentials and recovery.
  GitHub OAuth reuses an identity that already exists and has 2FA.
- **Unpinned HTMX from a CDN**: an external runtime dependency, and a supply-chain and CSP
  concern. Vendoring pins the exact file.

## Consequences

- hoster needs a GitHub OAuth App: a client id and secret in config or secrets, and a callback URL
  on the admin hostname.
- The UI must be reachable over HTTPS, so it needs a hostname, a route and a certificate. Exposure
  details are in ADR-0031.
- Session management, CSRF protection and how the admin is matched are settled in ADR-0031
  (handle comparison, with the rename risk accepted).
- Askama templates are compiled into the binary, which keeps ADR-0001's "one self-contained
  binary". Static assets (HTMX, CSS) are embedded too.
- HTMX 4 is a recent major version. Upgrading it means replacing a vendored file deliberately and
  checking the attribute and event changes from 2.x-era examples.
