---
id: ymb
title: "Live podman tests fail on podman 5.7.0 (Ubuntu 26.04): create conflict, exec and healthcheck answers differ from 6.1"
status: proposed
priority: P2
created: "2026-11-11T21:16:07.885022710Z"
updated: "2026-11-11T21:16:16.155323198Z"
tags:
  - runtime
  - podman
  - test
---

## Summary
Found while proving ghd (2026-11-11): the whole `runtime::podman` live module run against a stock Ubuntu 26.04.1 host (rootless podman 5.7.0, crun) through an ssh-forwarded socket: 36 passed, 5 failed. None involve stop/remove. CI's `podman` job runs only `e2e_tests` and the two config-files tests on `ubuntu-26.04`, so it never sees these; the tests were written against podman 6.1.4 (architecture §1.6).

1. `network_and_container_lifecycle_on_real_podman` (podman.rs ~2984): a duplicate container name is a **500** `container create: creating container storage: the container name "…" is already in use by <id>. You have to remove that container to be able to reuse that name: that name is already in use`, not 409, so `Call::create_error` gives `Api` instead of `Conflict`. **Possibly a real bug on 26.04** wherever hoster relies on `RuntimeError::Conflict` from create.
2. `exec_reports_exit_codes_output_and_cannot_run` (~3542): exec of a missing binary answers `crun: executable file \`pg_isready\` not found in $PATH: No such file or directory: OCI runtime attempted to invoke a command that was not found`; the test's expectation is 6.1's wording/shape.
3. `podman_never_runs_an_images_healthcheck` (~3904): inspect reports `Health { status: Some(""), failing_streak: Some(0), log: None }` where 6.1 reports none. Check whether hoster's healthcheck-off handling still holds (ADR-0056).
4. `a_timed_out_exec_is_killed_with_its_group` (~3582): "the exec is killed: Elapsed".
5. `an_exec_cut_off_at_any_point_is_ended_and_removed` (~3616): "300 ms: the exec is never ended".

4 and 5 may be an artefact of tunnelling the hijacked exec stream over `ssh -L`; rerun on the host itself (or on the CI runner) before treating them as 5.7 behaviour.

Note: the ADR-0059 OOM live test OOM-kills a container by design; on a desktop Ubuntu that pops a system OOM notification.

## Acceptance Criteria
- [ ] Each failure reproduced on 26.04 without a forwarded socket (e.g. run the module in CI's `podman` job, or on the host) and classified as test expectation vs. hoster bug.
- [ ] hoster bugs fixed (at least the create conflict, if it is one), tests accept both 5.7 and 6.1 answers, architecture §1 records the 5.7 differences.
- [ ] Decide whether CI's `podman` job should run the whole live module on 26.04.

## References
- ADR-0002, ADR-0056, ADR-0057 (podman 5.4 minimum); `docs/architecture.md` §1.6, §1.7
- `src/runtime/podman.rs` (`Call::error`, `Call::create_error`, the live tests), `.github/workflows/ci.yml` (`podman` job)