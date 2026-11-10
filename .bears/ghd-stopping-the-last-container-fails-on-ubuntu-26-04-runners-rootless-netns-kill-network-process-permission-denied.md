---
id: ghd
title: "Stopping the last container fails on ubuntu-26.04 runners: \"rootless netns: kill network process: permission denied\""
status: open
priority: P1
created: "2026-11-10T16:31:33.518839208Z"
updated: "2026-11-10T16:31:33.518839208Z"
tags:
  - runtime
  - ops
  - podman
---

## Summary
On GitHub's `ubuntu-26.04` runner (rootless podman 5.7.0), `ContainerRuntime::stop` of the only running container on a stack network answered HTTP 500:

```
container 6605…: removing container 6605… network: 1 error occurred:
	* rootless netns: kill network process: permission denied
```

Seen 2026-11-10 in PR #43's `podman` job (run 42854317111), in `config_files_reach_the_container_with_their_owner_and_mode`, at the stop before a restart. Everything before it passed, and the end-to-end deploy test in the same job passed. That test now keeps a second container running so its stops never empty the rootless network namespace; this task is about the failure itself.

Ubuntu 26.04 is a target host (ADR-0057 names it for `files`). If a real 26.04 host behaves like the runner, a stop that leaves no container running (a stop-first replace of a one-service stack, stack removal, the reconciler) would fail or report failure.

## Not known yet
- Whether a real Ubuntu 26.04 host does this, or only the runner image (AppArmor confinement of the rootless network helper is a guess, not verified).
- Whether the container is stopped despite the 500, and whether the namespace helper is left behind.
- Why the end-to-end test passes: its teardown may ignore the error.

## Acceptance Criteria
- [ ] Reproduced, or ruled out, on a real Ubuntu 26.04 host with rootless podman: start one container on a hoster network, stop it.
- [ ] If it reproduces: the cause is named, and either install.sh or doctor covers the host setup, or hoster treats this answer as a stopped container when inspect confirms it; architecture §1 records it.
- [ ] The anchor container in the config-files live test is removed again if stops work.

## References
- ADR-0002, ADR-0006, ADR-0023; `docs/architecture.md` §1.5
- `src/runtime/podman.rs` (`stop`, and the live test), `.github/workflows/ci.yml` (`podman` job)