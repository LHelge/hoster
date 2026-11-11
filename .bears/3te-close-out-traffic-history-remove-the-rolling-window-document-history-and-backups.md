---
id: "3te"
title: "Close out traffic history: remove the rolling window, document history and backups"
status: done
priority: P2
created: "2026-11-11T09:09:29.459216142Z"
updated: "2026-11-11T11:22:20.732836458Z"
tags:
  - stats
  - docs
  - observability
depends_on:
  - h6k
  - "5q8"
parent: t97
attempts: 1
---

## Summary

Once every page reads history, remove what it replaced and bring the documentation in line.

## Acceptance Criteria

- [x] The rolling `LatencyWindow`, `Stats::LATENCY_WINDOW`, the since-start `TrafficSnapshot` reads and any helper only they used are removed, unless a page still needs one (then say which and why in the commit). The in-flight gauge stays
- [x] Module docs in `src/stats/` describe the two parts that remain: live counters drained by the recorder, and history read by the pages
- [x] `spec.md` and the wiki (`site/content/wiki/`) say what the dashboard and stack pages show, that traffic history is kept (1 min for 24 h, 1 h for 60 d) and survives restarts, and that it is part of the database `hoster backup db` copies; `aphid build` in `site/` passes
- [x] A measured note in the PR description: database growth and flush time on a host with realistic traffic (or a test with a few dozen routes), to confirm ADR-0062's estimate
- [x] `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and the tests pass with nothing left `#[allow(dead_code)]` from this epic

References: ADR-0062, ADR-0045; `src/stats/`, `spec.md`, `site/content/wiki/` (backup guide).