---
id: y8n
title: Live exec test expects runc's wording; fails on crun
status: proposed
priority: P3
created: "2026-11-10T16:24:01.494088962Z"
updated: "2026-11-10T16:24:03.622284199Z"
tags:
  - runtime
  - test
---

## Summary
`runtime::podman::tests::exec_reports_exit_codes_output_and_cannot_run` fails on a podman that uses crun (found 2026-11-10 on rootless podman 6.1.4 with crun 1.30.1, on `main` as well as on `epic/xgf-config-files`). It asserts that the reason for a missing program contains `executable file not found`, which is runc's wording. crun says:

```
crun: executable file `pg_isready` not found in $PATH: No such file or directory: OCI runtime attempted to invoke a command that was not found
```

The test is gated by `TEST_HOSTER_PODMAN=1` and CI only runs `e2e_tests` and the config-files tests in its podman job, so CI does not see it. Ubuntu 26.04 and Debian 13 ship crun.

## Open question
Whether only the test's needle is wrong, or `ExecOutcome::CannotRun` detection in `src/runtime/exec.rs` also keys on runc's phrases and so misclassifies on crun (the test reached the `reason.contains` assertion, so the outcome was `CannotRun` here; the other two cases, `permission denied` and `is a directory`, were not reached).

## Acceptance Criteria
- [ ] The test passes on runc and on crun, for all three cases (missing program, not executable, a directory).
- [ ] If detection depends on the runtime's wording, architecture §1.6 records both.

## References
- ADR-0056; `docs/architecture.md` §1.6, §5.5
- `src/runtime/exec.rs`, `src/runtime/podman.rs` (the test)