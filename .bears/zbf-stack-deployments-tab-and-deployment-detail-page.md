---
id: zbf
title: Stack deployments tab and deployment detail page
status: done
priority: P2
created: "2026-11-08T18:20:48.393525472Z"
updated: "2026-11-08T22:07:23.148992952Z"
tags:
  - web
  - ui
depends_on:
  - "7wg"
parent: "8bf"
attempts: 1
---

Restyle `/stacks/{name}/deployments` as the deployments tab, after the "Deployments" section of `docs/design/StackDetail.dc.html`, and restyle the deployment detail page to match.

- **History table**: status pill (live / failed / cancelled / superseded / skipped / running), #id link, **trigger** (manual / webhook / poller, with the short commit sha and repository from provenance where present), digests (service + short digest, in mono), started (relative, absolute in `title`), duration (with the failure reason in muted text, like "1m 12s · healthcheck"), and **Redeploy** (labelled "roll back" for an older live deployment) as a ghost button with the existing confirm. A help line says that rollback redeploys the pinned digests and revision (ADR-0041).
- **Deployment detail** (`deployment.html` / `deployment_status.html`, the SSE-updated section): stack header with a breadcrumb `~/stacks/<name>/deployments/<id>`, state pill and duration, cancel/redeploy actions, error alert, target digests, provenance as a mono key/value list, and the transitions table. The live SSE update stays.
- The overview's "last deployment" links land here.

References: ADR-0040, ADR-0041, ADR-0047, `templates/deployments.html`, `templates/deployment.html`, `templates/deployment_status.html`, `src/web/stacks/deployments.rs`, `docs/design/StackDetail.dc.html`.