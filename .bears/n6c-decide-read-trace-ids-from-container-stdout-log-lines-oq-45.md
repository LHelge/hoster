---
id: n6c
title: "Decide: read trace ids from container stdout log lines (OQ-45)"
status: open
priority: P3
created: "2026-11-13T23:26:47.921185372Z"
updated: "2026-11-13T23:26:47.921185372Z"
tags:
  - decision
  - observability
  - runtime
---

## Decision
Should a JSON line a container prints to stdout link to its trace when it carries `trace_id` and `span_id`? Today only OTLP records and hoster's own records made inside a request have trace ids; the follower stores none for stdout lines, so they have no TRACE link and are not in "logs in trace". Task `9x4` asked for stdout records with ids to link, and that part is not built.

Options, recommendation and background: `docs/open-questions.md` OQ-45. Recommendation: B, read `trace_id` and `span_id` when they are valid W3C ids; no aliases yet.

## Done when
- The owner has chosen; ADR-0067 §3 is amended and a task is filed to implement it (B or C), or OQ-45 records that stdout stays unlinked (A).
- `site/content/wiki/tracing.md` says what the choice makes true.

## References
OQ-45, ADR-0067 (§3, §4), ADR-0069 (§6), `src/logs/follower.rs`, `src/logging/records.rs` (`trace_of`), task `9x4`.