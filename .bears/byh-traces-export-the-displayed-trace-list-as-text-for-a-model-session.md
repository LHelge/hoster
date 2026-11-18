---
id: byh
title: "Traces export: the displayed trace list as text for a model session"
type: epic
status: done
priority: P3
created: "2026-11-18T09:41:18.581243417Z"
updated: "2026-11-18T10:10:04.808134485Z"
tags:
  - observability
  - web
---

## Summary
Export the trace list the Traces tab currently displays (its filters, sort and page) as compact, model-readable text, so it can be pasted into or piped to a model session: a **Download** link and a **Copy** button on the tab, with an "open as text" view as the no-script fallback.

Owner decisions (2026-11-18):
- "Selection" means the currently displayed filtered list, i.e. the tab's `TraceParams` as they stand (stack, since, q, sort, status, slower, page), not the picked trace alone and not a scatter brush.
- Both copy and download, sharing one formatter.
- Copy is a tiny own dependency-free `assets/copy.js`, served hashed and loaded only on the Traces tab, CSP unchanged; recorded as an amendment to ADR-0060.

ADR-0045 (no metrics export) does not apply: this is a UI download like the logs tab's (ADR-0067), not an endpoint for outside systems.

## References
- ADR-0060 (own scripts), ADR-0067 (log download), ADR-0069 §6
- `docs/architecture.md` §2, the "Admin web UI" row's **Traces** bullet
- `src/web/telemetry/traces.rs`, `src/web/logs/download.rs`