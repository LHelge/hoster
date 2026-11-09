---
id: "8bf"
title: "M10: Admin UI design pass"
type: epic
status: done
priority: P2
created: "2026-11-08T18:18:53.543022428Z"
updated: "2026-11-09T00:16:30.219905833Z"
tags:
  - web
  - ui
---

## Scope

Re-skin the admin UI (ADR-0021) after the owner's Claude Design mockups, which are kept in `docs/design/` (first task). They are reference designs: re-implement them in Askama + HTMX + `assets/hoster.css`, and don't copy their inline styles. This builds on the Lucide icon sprite (ADR-0053, task 9vz).

Branch: `epic/8bf-admin-ui-design-pass`, one commit per task.

## Owner decisions (2026-11-08, recorded as ADR-0054 in the first task)

- **Theme:** Catppuccin **Mocha** in dark mode and **Latte** in light mode, chosen by `prefers-color-scheme`. Tokens are CSS custom properties. Text never goes dimmer than `#9399b2` in Mocha (the README's contrast rule), with Latte's equivalent.
- **Fonts:** **JetBrains Mono** vendored as Latin-subset woff2 (OFL), served as a hashed immutable asset, for everything machine-derived (names, SHAs, numbers, paths, table headers). Prose uses `system-ui`. **Inter is not vendored.**
- **Shell:** a 232 px sidebar (brand, version and hostname, nav with counts, user and session) that stacks on top at narrow widths. Pages have a `~/path` breadcrumb and an h1 header with an actions slot.
- **Containers** leaves the main nav. Containers and their logs move into each stack's pages, and `/containers` redirects to `/stacks`.
- **Stack pages** are separate URLs that share one header and tab bar: overview, logs, environment, deployments. The danger zone is the last section of the overview.
- **CSP stays** `default-src 'self'` with no `unsafe-*`. So there are no inline `style=` attributes and no inline scripts. Meters, bars and sparklines are SVG drawn on the server (sizes as attributes) or `<meter>`. Filters and toggles are query parameters or htmx requests.

## Low-hanging extras included

Host facts on System, container uptime and allocated port in the services table, and bulk certificate actions (check all DNS, retry all failing).

## Not adopted from the design (conflicts with an ADR or hoster's model)

`exec` into containers; revealing secret values (ADR-0029); editing env, domains or settings in the UI (stack file and hoster.toml are the sources, ADR-0024/0027); git-repo/compose stack sources and the repo/branch/compose picker (ADR-0003/0027); GitHub App and HMAC webhook secret (ADR-0047); users & tokens (single admin, ADR-0021/0031); DNS-01 (ADR-0016); a UI rollback button (CLI-only, ADR-0052); release channels; a background "update available" badge (ADR-0052); auto-deploy toggle; "changes not deployed" (ADR-0049); memory limits (ADR-0038).

Features that need new backend are separate epic stubs tagged `design-followup`. Where the design draws data hoster doesn't have (24h charts, req/min, sparklines, recent events, CPU/memory), this epic leaves that part out rather than faking it.

## Acceptance Criteria

- [ ] Every page uses the new shell, tokens and components, in both dark (Mocha) and light (Latte)
- [ ] No information or action the current UI offers is lost; each has a home in the new layout
- [ ] CSP unchanged; all web tests pass; text contrast at least 4.5:1 in both themes
- [ ] Layout works at 375 px width (sidebar stacks, wide tables scroll inside their card)

## References

ADR-0021, ADR-0031, ADR-0045, ADR-0053, `docs/design/README.md`, `assets/hoster.css`, `templates/`, `src/web/pages.rs`, `src/web/assets.rs`.