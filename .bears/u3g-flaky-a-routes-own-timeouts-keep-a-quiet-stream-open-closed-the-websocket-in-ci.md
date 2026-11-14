---
id: u3g
title: "Flaky: a_routes_own_timeouts_keep_a_quiet_stream_open closed the WebSocket in CI"
status: proposed
priority: P2
created: "2026-11-14T23:23:11.312264685Z"
updated: "2026-11-14T23:23:24.659051504Z"
tags:
  - test
  - proxy
---

## Summary
CI run 43651099905 (first attempt, PR #83, 2026-11-14) failed `proxy::tests::limits::a_routes_own_timeouts_keep_a_quiet_stream_open` at limits.rs:79 with `UnexpectedEof: early eof` after 3.246 s. The test sets 1 s proxy-wide read/write defaults, upgrades a WebSocket on `stream.example.com` (route timeouts 1 h), idles 3 s and expects the echo. The connection was closed while idle, so some 1 s bound applied after all. The rerun passed it, and so did 30 runs alone and 4 runs of all 131 `proxy::` tests locally. The code it exercises was unchanged by #82/#83.

Suspects, none confirmed: `DownstreamLimits` keepalive (1 s in `SHORT`, set by `admit`) applying to an upgraded HTTP/1.1 connection in some Pingora path; the client read/write timeouts set in `request_filter` being reset when the 101 is written; the upstream connection's timeouts.

## Acceptance Criteria
- [ ] The cause is found (Pingora's upgraded-session handling of `set_keepalive`/`set_read_timeout`, or the harness), reproduced where possible (CPU-starved runner, `taskset`, many parallel harnesses)
- [ ] Fixed in hoster if real users could be cut off (a quiet WebSocket on a long-timeout route), or in the test if it is the harness
- [ ] The test passes 20 full `cargo nextest run --profile ci` runs in a row

## References
- `src/proxy/tests/limits.rs` (`a_routes_own_timeouts_keep_a_quiet_stream_open`), `src/proxy/mod.rs` (`DownstreamLimits::admit`), `src/proxy/router.rs` (`request_filter`), ADR-0011, architecture §9.1, task `82j` (related flakes)