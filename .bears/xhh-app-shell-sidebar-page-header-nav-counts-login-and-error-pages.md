---
id: xhh
title: "App shell: sidebar, page header, nav counts; login and error pages"
status: open
priority: P2
created: "2026-11-08T18:19:24.356993792Z"
updated: "2026-11-08T18:19:24.356993792Z"
tags:
  - web
  - ui
depends_on:
  - dfs
parent: "8bf"
---

Replace the top bar in `templates/base.html` with the design's sidebar shell (see any `docs/design/*.dc.html`, the `<nav aria-label="Main">` block).

- **Sidebar** (232 px, surface background, stacks on top at narrow widths): a brand mark (a Lucide glyph via a new `Icon::Brand`) with "hoster" and `v<version> · <hostname>`; nav items Dashboard, Stacks, Certificates, Settings, System with icons and `aria-current`. **Containers leaves the nav** (the page itself is folded away in the services-table task).
- **Nav counts**: number of stacks on Stacks, and failing certificates on Certificates (error or TLS pending), with an `aria-label` like "1 failing". The layout gathers these cheaply on every full page render. Don't add a System "update" badge (the update check is click-only, ADR-0052).
- **User block** at the bottom: avatar initial, GitHub login, "session · Nh left" from the session's `expires_at`, an icon-only log-out button with `aria-label`, and "Log out everywhere" kept reachable (for example as a small ghost link in the same block).
- **Page header** pattern every page uses: a mono `~/path` breadcrumb (with links on stack sub-pages), the h1, and a right-hand actions slot. Flash messages sit under the header.
- **Swap glyphs** in `src/web/icons.rs` where the design differs (for example Settings as sliders, Stacks as layers, Brand). The variants stay named by meaning.
- Restyle `login.html` and `error.html` to match (a centred card, without the sidebar on login).
- Drop the footer version line, which the sidebar now shows.

References: ADR-0054, ADR-0031 (sessions), `templates/base.html`, `src/web/pages.rs` (`Layout`, `Section`, `NavItem`), `src/web/icons.rs`, `src/web/auth/mod.rs`.