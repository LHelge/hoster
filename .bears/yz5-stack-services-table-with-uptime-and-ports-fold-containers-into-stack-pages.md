---
id: yz5
title: Stack services table with uptime and ports; fold /containers into stack pages
status: done
priority: P2
created: "2026-11-08T18:20:19.770389557Z"
updated: "2026-11-08T21:05:54.021447664Z"
tags:
  - web
  - ui
  - runtime
depends_on:
  - "7wg"
parent: "8bf"
attempts: 1
---

The services table of the stack overview, after `docs/design/StackDetail.dc.html` (Services section), plus the move away from the Containers page.

- **Columns**: service (mono); image ref with the short digest underneath (full digest in `title`) and a "CI-built" tag; state/health pill (healthy / unhealthy / crash looping / deploying / stopped, with the health detail as `title`); restarts; **port** (the allocated loopback port for routed services, `internal` otherwise); generation; **up** (time since the container's `State.StartedAt`, from inspect via the runtime trait, so it needs a fake for tests); actions: `logs` (to the logs tab with that service selected). No CPU/memory (the resource-usage follow-up), no exec or restart.
- **Containers fold-in**: everything `/containers` showed (the container name in mono, generation, state, logs link) is reachable from the stack services table. Keep old containers of a draining generation visible as extra muted rows. `/containers` redirects to `/stacks`. `/containers/{name}/logs` keeps working until the logs tab task redirects it. Update tests and any docs or site pages that mention the Containers page (`grep -rn -i containers site/content docs/spec.md`).

References: ADR-0026 (ports), ADR-0028, ADR-0038, ADR-0044, `src/web/containers.rs`, `templates/containers.html`, `src/runtime/`, `src/web/stacks.rs`, `docs/design/StackDetail.dc.html`.