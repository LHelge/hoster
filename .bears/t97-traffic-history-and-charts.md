---
id: t97
title: Traffic history and charts
type: epic
status: open
priority: P3
created: "2026-11-08T18:22:29.983359533Z"
updated: "2026-11-08T18:22:29.983359533Z"
tags:
  - design-followup
  - observability
  - web
---

Stub from the design pass (epic 8bf). Break down when picked up.

## What

The design's time-based views: the dashboard **time-range picker** (1h–30d) with **responses by status** as stacked hourly bars and **p50/p95 lines**; per-stack 24h charts, **sparklines** in the stacks table and cards, **req/min**, and "vs previous period" deltas on KPI tiles. See `docs/design/Main.dc.html`, `Stacks.dc.html` and `StackDetail.dc.html`.

## Why it isn't in the design pass

Today's statistics are counters since hoster started plus a rolling latency window of a few minutes (ADR-0045: "statistics reset when hoster restarts"). The charts need **persisted time series**.

## Size and shape

Large. Probably per-route (or per-stack) rollup buckets (for example 1 min kept for 24h, 1 h kept for 30 days) written from the `Traffic` counters into SQLite (or a ring file) on a timer. Latency needs mergeable histograms per bucket, not just p50/p95. Charts are server-drawn SVG (ADR-0054, CSP).

## Starts with a decision

Retention and granularity, storage (SQLite tables with reversible migrations vs a separate file), the per-route vs per-stack key, and the write cadence and its I/O cost. This amends ADR-0045, so it needs a new ADR.

References: ADR-0045, ADR-0054, `src/stats/`.