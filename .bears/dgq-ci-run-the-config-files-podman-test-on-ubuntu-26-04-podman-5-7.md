---
id: dgq
title: "CI: run the config-files podman test on ubuntu-26.04 (podman 5.7)"
status: done
priority: P2
created: "2026-11-10T15:21:06.344379894Z"
updated: "2026-11-10T16:42:06.970733147Z"
tags:
  - ci
  - runtime
  - test
depends_on:
  - rp5
parent: xgf
attempts: 1
---

## Summary
The live config-files test from rp5 needs podman 5.4 or later, and the `podman` job in `.github/workflows/ci.yml` only runs `e2e_tests` on `ubuntu-latest`. Pin that job to `ubuntu-26.04` (podman 5.7, rootless) and run the config-files test in it, so the upload is exercised on every PR and not only on the owner's machine (owner 2026-11-10). This is also the first rootless 5.x run, which ADR-0057 and architecture §1.7 list as untested.

## Acceptance Criteria
- [x] The `podman` job runs on `ubuntu-26.04`, named explicitly so the podman version does not move with `ubuntu-latest` (which migrates from 24.04 to 26.04 between 2026-10-19 and 2026-11-19).
- [x] The job fails, not skips, when `podman --version` is older than 5.4.
- [x] The job runs rp5's config-files test in addition to `e2e_tests`, with `TEST_HOSTER_PODMAN=1`. Both pass on the runner.
- [x] Architecture §1 and §1.7 record rootless podman 5.7 as verified for the archive upload, with the date, if the run confirms it. A difference from the spike is fixed in rp5's code or raised as an open question, not papered over in the test.

## Result
- PR #43's `podman` job ran `e2e_tests` and both config-files tests on rootless podman 5.7.0: 3 run, 3 passed, none skipped.
- The first run failed at a `stop`, not at an upload: the runner refuses to tear down the rootless network namespace when the last container stops (Bears ghd). The test keeps a second container running; the upload assertions needed no change.

## Edge Cases
- The existing e2e test behaves differently on podman 5.7 than on 4.9: fix it here if it is a test assumption; file a task if it is a product bug.
- The runner's rootless setup differs on 26.04 (socket path, subuid ranges, crun instead of runc): adjust the job's setup step, not the test.

## Testing
- The PR's own CI run is the test: the job is green with both tests executed (check the nextest output lists them as run, not skipped).

## References
- ADR-0057 (§9, Consequences)
- `docs/architecture.md` §1 (Verified against table), §1.7
- `.github/workflows/ci.yml` (`podman` job), `src/e2e_tests.rs`, rp5's test in `src/runtime/podman.rs` or `src/e2e_tests.rs`