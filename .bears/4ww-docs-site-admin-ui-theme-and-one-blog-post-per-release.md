---
id: "4ww"
title: "Docs site: admin UI theme and one blog post per release"
status: done
priority: P2
created: "2026-11-09T07:32:06.413946682Z"
updated: "2026-11-09T08:49:55.421239127Z"
tags:
  - docs
attempts: 1
---

Give the aphid docs site (site/) a custom theme that matches the admin UI (ADR-0054 tokens: Catppuccin Latte/Mocha, JetBrains Mono, Lucide glyphs, sidebar shell, cards, pills), and use aphid's blog for one post per release, backfilled for v0.1.0–v0.2.0.

## References
- docs/adr/0054-admin-ui-design-system.md
- assets/hoster.css (the token source)
- site/aphid.toml, site/content/
- docs/adr/0052-release-install-and-self-update.md (release flow)

## Acceptance
- `aphid build` in site/ passes (no broken wiki-links).
- Light and dark mode both follow the admin UI tokens.
- /blog/ lists a post for each tag v0.1.0, v0.1.1, v0.1.2, v0.1.3, v0.2.0.