---
id: "5pm"
title: "Settings: grouped sub-nav, registries, read-only views of the effective config"
status: done
priority: P2
created: "2026-11-08T18:21:22.658708465Z"
updated: "2026-11-08T22:50:39.188982602Z"
tags:
  - web
  - ui
  - config
depends_on:
  - xhh
parent: "8bf"
attempts: 1
---

Redesign Settings after `docs/design/Settings.dc.html`. **Only registries stay editable.** Everything else is a read-only view of what hoster runs with, because config is `hoster.toml` plus `HOSTER_*` env (ADR-0024), and editing it in the UI is not adopted (epic 8bf).

- `/settings` redirects to the first section. A **sub-nav** (mono, grouped) splits into: INSTANCE: general; DEPLOY: registries, deploy webhook; NETWORK: reverse proxy, certificates. Each section is its own URL (`/settings/<id>`), and the main nav "Settings" is current for all of them.
- **general**: admin hostname, admin GitHub handle, public addresses (and where they came from), data and config paths, the config file path, with a note on which fields come from env.
- **registries**: the existing page in the field/row layout (label + help on the left, control on the right), with test/delete as ghost buttons.
- **deploy webhook**: the webhook URL to call from CI, the expected OIDC issuer/audience, and a link to the site's "deploying from CI" guide. No secret: auth is OIDC only (ADR-0047).
- **reverse proxy**: HTTP/HTTPS listen addresses, upstream port range, default timeouts and drain timeout, and the edge behaviour (421 / 404, ADR-0032) as text.
- **certificates (ACME)**: directory URL (prod/staging/custom shown as the radio-card look, read-only), contact email, renewal threshold, backoff, account key location.
- Each read-only section has a muted footer line: "Set in hoster.toml (or `HOSTER_…`); restart hoster to apply." **Never render secret values** (OAuth client secret, keys). Cover that with a test.

References: ADR-0024, ADR-0025, ADR-0031, ADR-0032, ADR-0047, `src/config.rs`, `src/web/settings.rs`, `templates/registries.html`, `site/content/wiki/`, `docs/design/Settings.dc.html`.