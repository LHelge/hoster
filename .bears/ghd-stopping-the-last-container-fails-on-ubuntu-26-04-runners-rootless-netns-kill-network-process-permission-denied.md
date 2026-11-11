---
id: ghd
title: "Stopping the last container fails on ubuntu-26.04 runners: \"rootless netns: kill network process: permission denied\""
status: done
priority: P1
created: "2026-11-10T16:31:33.518839208Z"
updated: "2026-11-11T21:17:13.540855566Z"
tags:
  - runtime
  - ops
  - podman
attempts: 1
---

## Summary
On GitHub's `ubuntu-26.04` runner (rootless podman 5.7.0), `ContainerRuntime::stop` of the only running container on a stack network answered HTTP 500:

```
container 6605…: removing container 6605… network: 1 error occurred:
	* rootless netns: kill network process: permission denied
```

Seen 2026-11-10 in PR #43's `podman` job (run 42854317111), in `config_files_reach_the_container_with_their_owner_and_mode`, at the stop before a restart. That test kept a second "anchor" container running so its stops never emptied the rootless network namespace.

## Findings (2026-11-11, stock Ubuntu 26.04.1 VM)
- Reproduces on a real host, CLI and compat API alike, for a stop and for a forced remove of the last running container on the namespace; `podman unshare --rootless-netns true` reproduces it alone.
- Cause: 26.04's apparmor ships `/etc/apparmor.d/podman` (unconfined, but labels podman `podman`), and passt's `usr.bin.pasta` profile has no `signal` rule for that peer: `apparmor="DENIED" operation="signal" profile="pasta" signal=term peer="podman"`. Fixed upstream in passt ("apparmor: Fix signaling issues from podman", Ubuntu LP #2154379), not yet in 26.04's archive.
- The calls did their job: inspect says exited / 404, pasta and the netns dir are gone, a later start works. `signal (receive) peer=podman,` in the pasta profile ends the 500.
- Fix (owner's choice: tolerate in hoster, no host changes): `stop`/`remove` treat a 500 mentioning `rootless netns` as done when inspect confirms it, with a warning.

## Acceptance Criteria
- [x] Reproduced, or ruled out, on a real Ubuntu 26.04 host with rootless podman: start one container on a hoster network, stop it.
- [x] If it reproduces: the cause is named, and either install.sh or doctor covers the host setup, or hoster treats this answer as a stopped container when inspect confirms it; architecture §1 records it.
- [x] The anchor container in the config-files live test is removed again if stops work.

Follow-up: the rest of the live module on podman 5.7 (ymb).

## References
- ADR-0002, ADR-0006, ADR-0023; `docs/architecture.md` §1.5
- `src/runtime/podman.rs` (`stop`, `remove`, `Call::failed_netns_teardown`, the live tests), `.github/workflows/ci.yml` (`podman` job)