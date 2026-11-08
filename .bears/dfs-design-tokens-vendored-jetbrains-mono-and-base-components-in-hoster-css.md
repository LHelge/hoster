---
id: dfs
title: Design tokens, vendored JetBrains Mono and base components in hoster.css
status: done
priority: P2
created: "2026-11-08T18:19:15.316536146Z"
updated: "2026-11-08T19:11:02.093069204Z"
tags:
  - web
  - ui
depends_on:
  - qde
parent: "8bf"
attempts: 1
---

The foundation every page task builds on. Existing pages must keep rendering, restyled by the new base rules.

- **Tokens** on `:root`: Catppuccin Latte by default and Mocha under `prefers-color-scheme: dark`. Name tokens by role (`--bg`, `--surface`, `--inset`, `--border`, `--divider`, `--input-border`, `--text`, `--text-2`, `--muted`, `--accent`, `--on-accent`, `--ok`, `--progress`, `--warn`, `--fail`, `--p50`, `--p95`, plus `*-tint` backgrounds at about 10% alpha), so pages never name a Catppuccin colour. Check text and status colours against 4.5:1 on their backgrounds in **both** flavours. Latte's yellow, peach and green are weak on white, so pick darker Latte shades where needed and note the choice in a CSS comment.
- **JetBrains Mono**: vendor Latin-subset woff2 (regular, medium, semibold) under `assets/` with its OFL licence, an entry in `assets/VENDORED.md`, and served through `src/web/assets.rs` as hashed `immutable` assets (`font/woff2`). `@font-face` with `font-display: swap`. Prose stays `system-ui`. Make sure the CSP still passes (`default-src 'self'` covers fonts; the test in `src/web/tests.rs` must stay green).
- **Base components** as classes, matching the design README: cards/sections (10–12 px radius, surface background), table styles (11 px uppercase mono headers with .06em tracking, inner dividers, a scroll wrapper inside a card), buttons (primary in accent with `--on-accent` text, `ghost`, `danger`; 8 px radius; 44 px min height for primary actions), form controls on an inset background, focus rings in accent, status **pills** (status colour text on its tint), KPI tiles, `.mono`.
- **`Tone` type** in the web module (`Ok`, `Progress`, `Warn`, `Fail`, `Neutral`), with the states that already exist (deployment state, health, cert status, DNS result) mapping into it via `From`, and a pill that renders itself (`HtmlSafe`, like `Icon`) with icon + text. Replace the `badge-*` and `state-failed` classes with it.
- **Meter**: a small type that renders a server-drawn SVG bar (value/max as attributes, tone colour via class), with no inline style. It's used later for cert days-left, slowest routes and similar.

References: ADR-0054, ADR-0053, `docs/design/README.md` (tokens), `assets/hoster.css`, `assets/VENDORED.md`, `src/web/assets.rs`, `src/web/icons.rs`.