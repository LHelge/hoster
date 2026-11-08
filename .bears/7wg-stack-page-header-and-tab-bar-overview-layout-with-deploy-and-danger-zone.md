---
id: "7wg"
title: Stack page header and tab bar; overview layout with deploy and danger zone
status: done
priority: P2
created: "2026-11-08T18:19:54.956592362Z"
updated: "2026-11-08T20:21:34.496866951Z"
tags:
  - web
  - ui
depends_on:
  - xhh
parent: "8bf"
attempts: 1
---

Shared frame for all stack pages, after the header of `docs/design/StackDetail.dc.html`. This task moves the existing content of `/stacks/{name}` into the new layout. Later tasks add new sections.

- **Stack header** (an Askama partial or a type used by every stack page): breadcrumb `~/stacks/<name>[/tab]`, the mono stack name, a status pill ("running · 3/3 healthy" and so on), and a mono line with the revision and hash, applied time, and the last deployment (#id, state, source, relative time; the short sha from provenance for webhook deploys), plus the active or queued deployment.
- **Header actions**: **Redeploy** (redeploys the last live deployment, using the existing redeploy endpoint and confirm) and **Re-resolve tags** (only for third-party images; result shown inline, as today). Deploying explicit digests stays a form section on the overview, not a header button.
- **Tab bar** (`role="tablist"`, links): overview `/stacks/{name}`, logs `/stacks/{name}/logs`, environment `/stacks/{name}/secrets` (or a renamed route with a redirect), deployments `/stacks/{name}/deployments`. Tabs whose pages are not restyled yet still link to the existing pages.
- **Overview body**, for now: a services table in the new table style (the services task extends it), the **Deploy** form for CI-built services, "Apply a new revision" as a ghost link to `/apply`, **Refused webhook deploys** (table in a scroll wrapper), and a **Danger zone** card at the bottom (red-tinted border) with the existing remove form and the type-the-name confirm. Its text names what goes (containers, network, routes) and what stays (revisions, history, volumes unless ticked), and lists the stack's volumes.

References: ADR-0017, ADR-0041, ADR-0054, `templates/stack.html`, `src/web/stacks.rs`, `src/web/stacks/deployments.rs`, `docs/design/StackDetail.dc.html`.