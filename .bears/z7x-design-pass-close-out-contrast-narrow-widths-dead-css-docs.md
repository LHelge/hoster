---
id: z7x
title: "Design pass close-out: contrast, narrow widths, dead CSS, docs"
status: open
priority: P2
created: "2026-11-08T18:21:30.066090727Z"
updated: "2026-11-08T18:21:30.066090727Z"
tags:
  - web
  - ui
  - docs
depends_on:
  - mtn
  - "75z"
  - cu8
  - yz5
  - gn6
  - awz
  - zbf
  - sak
  - "5pm"
  - "8qb"
  - uf5
parent: "8bf"
---

Final check across all pages before the epic PR.

- Run the app (`/run` skill or a local dev setup) and look at every page in **Mocha and Latte**, at 1440 px and at **375 px**: the sidebar stacks, wide tables scroll inside their card, no page-level horizontal scroll, primary actions at least 44 px.
- Contrast: check text, pills and muted text at 4.5:1 or more in both flavours (a script over the tokens is fine). Visible focus rings on every interactive element; icon-only buttons have `aria-label`s.
- Remove CSS rules no template uses any more, and leftover `badge-*` / `state-failed` usages.
- Check that the epic's "nothing lost" criterion holds: every link, form and endpoint the pre-epic UI had is reachable (compare with `git diff main -- templates/`).
- Update user-facing docs that describe the UI (`site/content/wiki/`, `spec.md`): the Containers page is gone, stack tabs, settings sections. Run `aphid build` in `site/`. Screenshots are optional.

References: ADR-0054, epic 8bf acceptance criteria, `assets/hoster.css`, `site/content/wiki/`.