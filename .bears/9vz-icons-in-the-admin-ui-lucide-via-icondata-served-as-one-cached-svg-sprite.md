---
id: "9vz"
title: "Icons in the admin UI: Lucide via icondata, served as one cached SVG sprite"
status: in_progress
priority: P2
created: "2026-11-08T17:36:13.652227767Z"
updated: "2026-11-08T17:36:19.267803125Z"
tags:
  - web
  - ui
attempts: 1
---

Owner decisions (2026-11-08, recorded as ADR-0053): Lucide from the `icondata` crate (only the `lucide` feature), delivered as one generated SVG sprite at a content-hashed `/assets/icons.<hash>.svg` with the same immutable one-year cache as the other assets, referenced with `<svg><use href="…#id"/></svg>` so icons inherit `currentColor` in light and dark mode.

Scope before the design pass:
- An `Icon` type named by meaning (Dashboard, Stacks, Warning, Redeploy, …), not by glyph, so the design pass swaps glyphs in one place. It renders its own markup in Askama templates (`HtmlSafe`).
- Icons on the main nav, on status badges (ok / warning / failed / pending) and on key action buttons (apply, deploy, redeploy, remove/delete, update, log out).
- Lucide's ISC licence recorded next to the other vendored assets.

References: ADR-0021, `src/web/assets.rs`, `assets/VENDORED.md`, `templates/base.html`, `src/web/pages.rs`.