---
id: hzc
title: "Sidebar host meters: cpu, mem and disk with sparklines, refreshed by an HTMX poll"
status: open
priority: P2
created: "2026-11-12T00:08:17.739858239Z"
updated: "2026-11-12T00:08:17.739858239Z"
tags:
  - web
  - observability
depends_on:
  - bqt
parent: qmw
---

## Summary

The **HOST** card at the bottom of the sidebar on every signed-in page (every screen in `docs/design/`, e.g. `StackDetail.dc.html` lines 68–81): cpu `23%`, mem `11.2 / 32 GB`, disk `412 / 1000 GB`, each with a server-drawn meter, fed by the resource sampler (task bqt, ADR-0066).

## What

- The card sits between the nav and the account block in `templates/base.html`, headed `HOST · <hostname>`. It is its own template fragment, served also at a small authenticated GET endpoint; the card polls that endpoint with HTMX every 10 s (`hx-trigger="every 10s"`, swapping itself), so the meters stay live on any page without a second SSE connection.
- Rendering a page reads only the sampler's cache (never podman, never `/proc`).
- Each row: label, value in JetBrains Mono, a `Meter` (`src/web/ui.rs`) with the tone thresholds of ADR-0066, and for cpu and mem a `Sparkline` (`src/web/charts.rs`) of the last hour. Disk is the filesystem of podman's storage root. An unknown value shows `—` with an empty meter; the card never fails a page.
- Byte sizes are formatted consistently with how the UI already prints sizes (see the System page and `ByteSize`); the mockup's `GB` is illustrative.
- No inline styles (CSP, ADR-0054); new CSS goes in the stylesheet under `assets/` with the existing tokens, light and dark. The site theme in `site/theme/` copies tokens only; leave it unless a token changes.
- The System page's host card shows the data dir's filesystem usage too when it is a different filesystem from podman's storage (ADR-0066), if that is small; otherwise leave it and say so in the task.
- `ui_preview.rs` (the component preview), if it lists shell parts, gets the card.

## Acceptance Criteria

- [ ] Every signed-in page shows the card; the login and error pages for signed-out users do not
- [ ] The fragment endpoint requires a session and returns only the card
- [ ] Web tests: values and meters from a fixed sampler state, the unknown state, the poll attributes
- [ ] The user-facing docs mention the meters where the admin UI is described, if such a page exists in `site/content/wiki/`
- [ ] `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

References: ADR-0066, ADR-0054, ADR-0021; `docs/design/StackDetail.dc.html` (sidebar only), `templates/base.html`, `src/web/pages.rs`, `src/web/ui.rs`, `src/web/charts.rs` (`Sparkline`), `src/web/system/host.rs`.