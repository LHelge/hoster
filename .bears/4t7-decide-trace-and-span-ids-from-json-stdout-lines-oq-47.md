---
id: "4t7"
title: "Decide: trace and span ids from JSON stdout lines (OQ-47)"
status: done
priority: P3
created: "2026-11-17T20:10:43.882835234Z"
updated: "2026-11-17T20:53:27.256886683Z"
tags:
  - decision
  - observability
---

## Summary
Decide whether the container log follower reads `trace_id` / `span_id` from JSON stdout lines into the record columns, so stdout records link to their trace like OTLP records do. Raised by epic `qnh` (task `9x4`) on 2026-11-17: the views link any record that has ids, but only the OTLP receiver fills them.

## Acceptance Criteria
- [x] The owner has chosen among the options of OQ-47
- [x] The choice is recorded as an ADR (an amendment to ADR-0067's JSON mapping) and OQ-47 points to it
- [x] If the follower is to read the keys, an implementation task exists (`3ku`)

## References
- `docs/open-questions.md` OQ-47, ADR-0067 (§4, JSON mapping), ADR-0069
- `src/logs/follower.rs`, `site/content/wiki/tracing.md`