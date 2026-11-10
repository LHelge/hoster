---
id: "2eq"
title: Require podman 5.4 for stacks with `files`; doctor reports the podman version
status: open
priority: P2
created: "2026-11-10T15:20:41.595347468Z"
updated: "2026-11-10T15:20:41.595347468Z"
tags:
  - runtime
  - orchestrator
  - ops
depends_on:
  - nnq
parent: xgf
---

## Summary
Implement ADR-0057 §9: a stack with `files` is rejected at apply, and a deployment of one fails in `Validating`, on a podman older than 5.4. Stacks without `files` are unaffected. `hoster doctor` reports the podman version. This lands before the upload (rp5), so the upload code never runs against a podman it was not verified on.

## Acceptance Criteria
- [ ] A podman version type with `FromStr` / `Display` / `Ord` parses what podman reports (`6.1.4`, `5.4.2`, `4.9.3`, a `-rc1` or `-dev` suffix). `PodmanRuntime` keeps the version it already reads at connect (`src/runtime/podman.rs`, today only logged), `ContainerRuntime` exposes it, and the fake can be set to any version.
- [ ] Apply: a stack where any service has `files` is rejected on a podman older than 5.4, naming the running and the required version, e.g. "config files (`files`) need podman 5.4 or later; this host runs podman 4.9.3". The apply page shows it like other apply errors.
- [ ] `Validating`: a deployment of such a stack fails with the same message (the podman may have been downgraded, or the revision restored from a backup).
- [ ] Stacks without `files` apply and deploy on any podman, as today.
- [ ] `hoster doctor` reports the podman version as a passing check, and says in its detail when it is older than 5.4 that stacks cannot use `files`. It does not fail on an old podman.

## Edge Cases
- A version hoster cannot parse: the stack with `files` is rejected, quoting the raw string (planner's reading; ADR-0057 does not say). Stacks without `files` are still unaffected.
- A rollback to a revision with `files` on an old podman fails in `Validating`, not at create.
- Doctor with the socket down: the version check is skipped or reported unknown; the existing socket check carries the failure.

## Testing
- Unit tests for parsing and ordering (`5.4.0` passes, `5.3.9` and `4.9.3` fail, suffixes).
- Fake-runtime tests: apply rejected and `Validating` failed on 4.9.3, both naming both versions; the same stack accepted on 5.4.0; a stack without `files` accepted on 4.9.3.
- A doctor test for the version line.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0057 (§9, Consequences)
- `docs/architecture.md` §5.1 (Validating), §1.7 (verified podman versions)
- `src/runtime/mod.rs`, `src/runtime/podman.rs`, `src/runtime/fake.rs`, `src/orchestrator/mod.rs` (apply), `src/orchestrator/engine.rs` (Validating), `src/doctor/mod.rs`, `src/web/stacks/apply.rs`