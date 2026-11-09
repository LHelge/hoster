---
id: tt8
title: Latency is time to response header; dashboard traffic tiles leave out the admin UI
status: done
priority: P1
created: "2026-11-09T20:46:11.028292980Z"
updated: "2026-11-09T20:51:46.023605928Z"
tags:
  - proxy
  - stats
  - bug
attempts: 1
---

## Problem

The dashboard's p95 latency reads ~29 s while p50 is a few ms. The proxy records each request's
latency as the time from reading the request header to the *end* of the request
(`AccessRecord::duration`, `src/proxy/router.rs`), so every SSE stream, WebSocket and long
download counts as a request as slow as it was long-lived. The admin UI's own SSE streams
(`/events`, `/stacks/{name}/events`, deployment progress, log follow) feed the dashboard total,
and with a 5-minute window on a low-traffic host a few closed streams are more than 5 % of the
samples.

## Decisions (owner, 2026-11-09)

1. Latency in the statistics is **time to response header**: from reading the request header to
   the moment the upstream's response header is ready to go to the client. The access log keeps
   the total duration.
2. The dashboard's traffic tiles (requests, 5xx rate, p50, p95) show the health of the hosted
   services: they **leave out hoster's own reserved routes** (the admin UI). The "Outside the
   stacks" note under the stacks table still counts them. Unrouted requests stay in the tiles.

## Acceptance

- [x] `RequestOutcome::latency` is the time to the response header for proxied requests; requests
      hoster answers itself or that fail before a response keep the total duration
- [x] The access log's `duration_ms` is unchanged (total)
- [x] A proxy test: a long-lived stream (SSE) is counted with a latency far below its lifetime
- [x] The dashboard tiles do not count `Owner::Reserved` routes
- [x] Docs that define the latency figure say what it measures (`src/stats`, ADR-0045)

## References

- `src/proxy/router.rs` (`RequestCtx`, `response_filter`, `logging`, `AccessRecord`),
  `src/stats/traffic.rs`, `src/web/dashboard.rs` (`kpis`)
- ADR-0045