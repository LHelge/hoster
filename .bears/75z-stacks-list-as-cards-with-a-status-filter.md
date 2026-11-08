---
id: "75z"
title: Stacks list as cards with a status filter
status: done
priority: P2
created: "2026-11-08T18:19:43.636437729Z"
updated: "2026-11-08T19:52:13.150177389Z"
tags:
  - web
  - ui
depends_on:
  - xhh
parent: "8bf"
attempts: 1
---

Redesign `/stacks` after `docs/design/Stacks.dc.html`.

- **Header**: "New stack" as the primary action (to `/apply`).
- **Status filter** as a group of pressed-state buttons (all / running / degraded / deploying / failed, each with a count). These are links that set `?status=`, rendered on the server. There's no text filter and no card/table toggle (the dashboard is the table view).
- **Card per stack**: name (mono link) with a status pill; a source line with the **revision number and short hash**, and for the last deployment its source (manual / webhook / poller), relative time, and for webhook deploys the short commit sha and repository from the OIDC provenance; service chips with a health dot (ok / failing / deploying / stopped); routed hosts (`↗ host/prefix`); a mini stats row with requests since start, p95 and 5xx share. Keep the flag text, the active deployment and "#n queued" that the current table shows.
- A dashed **"Add a stack"** card at the end, linking to `/apply` ("apply a stack file").
- Cards sit in a responsive grid that goes to one column at narrow widths.

References: ADR-0027, ADR-0047 (provenance), ADR-0054, `templates/stacks.html`, `src/web/stacks.rs`, `docs/design/Stacks.dc.html`.