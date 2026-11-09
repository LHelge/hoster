---
id: "6vr"
title: Flaky tests failed the v0.3.0 release build on aarch64
status: done
priority: P1
created: "2026-11-09T21:42:28.288478705Z"
updated: "2026-11-09T22:22:11.193435529Z"
tags:
  - test
  - flaky
  - ci
attempts: 1
---

## Problem

The release workflow for v0.3.0 (run 42703218391, 2026-11-09) failed twice in the aarch64
`build, test, package` job, each time on a different test that passes on PR CI and locally. The
release workflow runs the tests, so a flake blocks a release (and fail-fast cancels x86_64).

1. `orchestrator::deploy_events_tests::a_cancelled_deploy_is_a_warning`
   (`src/orchestrator/deploy_events_tests.rs:220`): `cancelled.error` was `None`, expected
   `Some("cancelled")`.
2. `a_busy_http_port_fails_startup_in_step_7` (`tests/supervisor.rs`): `AddrInUse` when the test
   binds `sandbox.http`.

## Causes and fixes

1. A stale read in `Orchestrator::wait`, not in the test. `wait` read the deployment record and
   then checked whether the deployment still held its stack's slot. A deployment that ended
   between the two was returned as the non-terminal record read before it ended (here
   `Aborting`, with no error yet). `wait` now reads the record again once the slot is let go.
   Reproduced on the old code 3 times in 1,152 runs; 0 in 3,864 with the fix.
2. `Sandbox::new` picked its ports by binding `127.0.0.1:0` and dropping the listeners. Until
   hoster (or the test) bound them again, the kernel could hand the same ephemeral port to any
   socket of a test running in parallel. The same race failed
   `a_second_sigterm_during_the_grace_period_exits_promptly` once in 210 local runs of the
   file. The sandbox now takes its ports from below the ephemeral range, by number, and the
   busy-port test keeps the listener that picked the port. 0 failures in 200 runs of the file.

## Acceptance

- [x] Each test passes 200 consecutive runs
- [x] The cause of (1) is written down in the commit message

## References

- `src/orchestrator/mod.rs` (`wait`), `tests/supervisor.rs` (`Sandbox`)
- `.github/workflows/release.yml`