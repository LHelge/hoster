---
id: uf5
title: "Apply page as \"New stack\": editor with a Plan and Pre-flight aside"
status: open
priority: P2
created: "2026-11-08T18:21:22.678694309Z"
updated: "2026-11-08T18:21:22.678694309Z"
tags:
  - web
  - ui
depends_on:
  - xhh
parent: "8bf"
---

Restyle `/apply` and its preview after the layout of `docs/design/NewStack.dc.html`. Stacks still come from a pasted or uploaded stack file (ADR-0027). The repo/branch/compose picker is not adopted.

- **Header**: breadcrumb `~/stacks/new`, h1 "Apply a stack file" (the stacks page's "New stack" button lands here).
- **Left column**: a numbered "1 Stack file" section with the mono textarea on an inset background and the upload control. "Check" is the primary button.
- **Right aside** (sticky on wide screens, below on narrow ones), filled by the htmx preview: **Plan** as a `+ / ~ / −` list from the preview (creates or replaces the stack, services to redeploy, third-party pins and their changes, routes/hosts that appear or go away, if the preview already knows them); **Pre-flight** with ✓ / ! lines from the preview's warnings and problems; then the confirm button ("Apply and redeploy n services"), and cancel. The stack-file **diff** goes in a full-width card under the two columns.
- Problems (`problems.html`) render as a fail-tinted card with the source excerpt in mono.
- Don't add new checks (the DNS pre-flight is in the DNS-diagnostics follow-up).
- The re-resolve preview (`reresolve_preview.html`, `pins.html`) uses the same plan-list look.

References: ADR-0027, ADR-0049, `templates/apply.html`, `templates/apply_preview.html`, `templates/problems.html`, `templates/pins.html`, `templates/reresolve_preview.html`, `src/web/stacks.rs`, `docs/design/NewStack.dc.html`.