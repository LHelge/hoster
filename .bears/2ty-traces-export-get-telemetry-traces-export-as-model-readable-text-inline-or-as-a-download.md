---
id: "2ty"
title: "Traces export: GET /telemetry/traces/export as model-readable text, inline or as a download"
status: done
priority: P3
created: "2026-11-18T09:41:34.107633778Z"
updated: "2026-11-18T09:59:04.656953309Z"
tags:
  - observability
  - web
parent: byh
attempts: 1
---

## Summary
A route next to the Traces tab that renders the traces the tab displays for the same GET parameters (`TraceParams`: stack, since, q, sort, status, slower, page; `trace`, `span`, `collapsed` and `crit` are ignored) as plain text written for a model to read, plus a **Download** link and an **Open as text** link on the tab.

## Format (`text/plain; charset=utf-8`)
- A header: what this is (hoster trace export), the resolved UTC range, the stack (or every stack), the filters and sort in effect, "traces N–M of <matched>", and a one-line legend of the span-line fields.
- Then each trace of the displayed page, in the list's order: a summary line (trace id, start, root name, stack, duration, status, span count, log record count, "app not instrumented" for a proxy-only trace), then its span tree from `LogStore::load_trace` as an indented tree (box-drawing or two-space indent), one line per span: service, name, kind, duration, offset from the trace start, status and status message, then its attributes inline as `key=value` with values cut at a fixed length. Resource attributes are listed once per service per trace, not per span. Span events and the span's log records go on indented lines under their span (time, level, message).
- Caps as named constants, each ending with a line that says what was left out: a total span budget over the export (on the order of a few thousand lines), a per-value length, and log records per span. Traces past the span budget still get their summary line.
- Nothing is an HTML escape. Strings that hold line breaks are written with `\n`, as the log download does.

## Acceptance Criteria
- [x] `GET /telemetry/traces/export?<the tab's params>` behind the admin session returns the format above for exactly the traces the tab lists for those params (same `TraceQuery`, built in one place, not duplicated)
- [x] `download=1` adds `Content-Disposition: attachment` with a file name built from the stack and the resolved range only (never a parameter as given), e.g. `traces-<stack|all>-<start>-<end>.txt`; without it the text is served inline
- [x] Bad parameters fall back as the tab does, and the fallback is noted in the header rather than failing the request
- [x] The tab shows **Download** and **Open as text** links beside the list, carrying the current list params (`TraceParams` builds them), only when the list has traces
- [x] Tests: the format on a small fixed trace (snapshot-style string compare), each cap's cut line, params mapping to the same query as the page, the file name, a missing or unreadable store
- [x] `docs/architecture.md` §2's **Traces** bullet mentions the export; `site/content/wiki/tracing.md` gets a short "Exporting traces" section

## Implementation Notes
- Build the export on a type (e.g. `TraceExport`) in a module under `web/telemetry/traces/`, with `Display` for the text, and keep its public surface to the handler
- Read the page with `LogStore::traces`, then each trace with `LogStore::load_trace` (sequentially or with a small bounded concurrency), stopping span loads once the budget is spent
- Precedent for headers, the file name and the cap line: `src/web/logs/download.rs`
- The trace list is unbounded on busy ranges (`ybg`); the export reads one page, so it adds at most a page of `load_trace` calls

## References
- ADR-0067 (download), ADR-0069 §6
- `docs/architecture.md` §2, the "Admin web UI" row's **Traces** bullet
- `src/web/telemetry/traces.rs`, `src/store/logs/trace.rs`, `src/store/logs/query/traces.rs`, `src/web/logs/download.rs`