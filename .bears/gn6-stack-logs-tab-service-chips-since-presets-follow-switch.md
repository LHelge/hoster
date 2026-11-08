---
id: gn6
title: "Stack logs tab: service chips, since presets, follow switch"
status: open
priority: P2
created: "2026-11-08T18:20:27.413788209Z"
updated: "2026-11-08T18:20:27.413788209Z"
tags:
  - web
  - ui
  - observability
depends_on:
  - yz5
parent: "8bf"
---

Move container logs (ADR-0044) to `/stacks/{name}/logs`, after `docs/design/StackLogs.dc.html`, keeping today's one-container-at-a-time stream.

- The stack header and tab bar, with "logs" selected.
- A toolbar (mono, on an inset background) with **service chips**, one per service with a coloured dot, links that set `?service=`. The default is the first service. The service's current container is streamed. When a draining generation exists, show a small selector for which container. Also: **since** presets (15m / 1h / 6h / 24h / all) plus the existing free text, **tail**, and a **follow** switch. These are GET form fields rendered on the server; the SSE stream is unchanged.
- The log view: dark inset panel, mono 12–13 px, time column muted. Stderr lines get the fail tint with a left border, replacing today's legend. A footer line shows the line count and "following" state where known.
- Leave out the merged all-services stream, grep, level, download, and the access-log/deploy sources (the stack-logs follow-up).
- `/containers/{name}/logs` redirects to the matching stack logs URL.

References: ADR-0044, `templates/container_logs.html`, `templates/log_lines.html`, `src/web/containers.rs`, `docs/design/StackLogs.dc.html`.