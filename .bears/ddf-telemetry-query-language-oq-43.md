---
id: ddf
title: Telemetry query language (OQ-43)
type: epic
status: open
priority: P3
created: "2026-11-12T19:38:02.444638600Z"
updated: "2026-11-12T19:38:02.444638600Z"
tags:
  - observability
  - web
depends_on:
  - psq
---

Stub from planning the telemetry epics on 2026-11-12. Not broken down: it starts with the decision task, and only once the Logs and Traces tabs are in use.

## What

`docs/design/Telemetry.dc.html` has a query box per tab that takes expressions (`service.name = "outline-app" AND duration > 500ms AND status = error`, `body ~ "timeout" AND severity >= WARN`). For now the owner chose filter controls plus grep (OQ-43, interim answer). If the decision says yes, this epic adds a small `field op value AND …` language, parsed in Rust into the same query types the controls fill (`LogQuery`, `RequestQuery`, `TraceQuery`), with the controls writing terms into the box and the box round-tripping to a URL.

## Why later

Which filters people type rather than click is only clear once the tabs are used. A language is a parser, an error UX and docs to maintain.

## Size and shape

Medium if option B of OQ-43: one parser, a mapping onto the three query types, error rendering, a help popover, a wiki section.

References: OQ-43, ADR-0067 (§8), `docs/design/Telemetry.dc.html`, epics `psq` and `qnh`.