---
id: "7v2"
title: "Flaky: stopping_the_proxy_frees_every_slot leaves a slot after a fast stop"
status: done
priority: P2
created: "2026-11-12T14:55:11.988893452Z"
updated: "2026-11-12T14:57:13.255188484Z"
tags:
  - test
  - proxy
  - flaky
---

## Summary
`proxy::tests::limits::stopping_the_proxy_frees_every_slot` failed in CI on PR #68 (run 43231161687, job 131226511603) with `assertion left == right failed: FastShutdown; left: 1, right: 0` at `src/proxy/tests/limits.rs:649`. The graceful case of the same test passed.

## Cause
On a fast stop Pingora shuts its runtimes down with `Runtime::shutdown_timeout(Duration::ZERO)` (pingora-core 0.9.1, `src/server/mod.rs`, `ShutdownType::Quick`). tokio's blocking pool then returns without waiting for the worker threads (`blocking/pool.rs`, `shutdown_rx.wait(Some(0))`), and they drop their remaining connection tasks afterwards. A slot is given back when Pingora drops the connection's L4 stream (`Slot::on_disconnected`), so one can come back just after the proxy thread has exited. A graceful stop waits for the workers, so it is not affected.

Not a product bug: the proxy runs once per process and hoster exits after stopping it, so nothing reads the count after a fast stop.

## Acceptance Criteria
- [x] The fast case waits for the count to settle (`slots_settle`, 5 s) before asserting; the graceful case keeps the immediate check
- [x] The test's doc comment explains why the two cases differ
- [x] fmt, clippy and the full suite pass

## References
- ADR-0065
- `src/proxy/tests/limits.rs`, `src/proxy/admission.rs`