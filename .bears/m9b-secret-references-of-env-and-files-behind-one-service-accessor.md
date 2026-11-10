---
id: m9b
title: Secret references of env and files behind one `Service` accessor
status: open
priority: P2
created: "2026-11-10T15:20:41.573575495Z"
updated: "2026-11-10T15:20:41.573575495Z"
tags:
  - stack
  - orchestrator
depends_on:
  - nnq
parent: xgf
---

## Summary
Implement the engine-side half of ADR-0057 §6 before any file reaches a container: one `Service` accessor yields every secret reference of a service, from `secrets` (env) and from `files`, and the existing readers of `Service.secrets` switch to it. After this task a secret used only by a file is versioned, checked and redeployed like an env secret. Revealing file secrets, redacting them and the 256 KiB cap need the upload and are rp5's.

## Acceptance Criteria
- [ ] One accessor on `Service` yields each secret reference with what uses it: an env var name or a file's `ContainerPath`. No caller outside `src/stack` reads `Service.secrets` and `Service.files` separately to find secrets.
- [ ] Version collection (`Orchestrator::secret_versions`, `src/orchestrator/mod.rs`) reads the accessor, so the config hash's secret-file entries (shape from nnq) carry the current version, and changing a file secret redeploys only its service (ADR-0057 §7).
- [ ] `secret_users` (`src/orchestrator/stack_secrets.rs`) reads the accessor: setting or deleting a secret that only a file references starts the ADR-0049 redeploy of that service, and the secret lists that service as a user.
- [ ] The missing-secret check (`check_secrets`, `src/orchestrator/engine.rs`) reads the accessor and rejects by name and use, never a value: `(for DATABASE_URL)` for env as today, `(for file /etc/app/credentials.json)` for a file.
- [ ] `Service::container_env`'s `PORT` rule keeps looking at env secrets only.

## Edge Cases
- The same secret used by an env var and a file of one service: one version entry, the service listed once as a user, and the missing-secret message names the first use.
- A secret used by a file in one service and by env in another: a change redeploys both, and nothing else.
- A stack without `files` behaves exactly as before; no config hash changes.

## Testing
- Unit tests for the accessor in `src/stack` (env only, files only, both, duplicates).
- Fake-runtime tests: a file-only secret change redeploys only its service; a deleted file secret rejects the deployment by name and path; `stack_secrets_tests.rs` covers the user list.
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- ADR-0057 (§6, §7); ADR-0029, ADR-0049
- `docs/architecture.md` §9 (secrets at rest)
- `src/stack/mod.rs`, `src/orchestrator/mod.rs`, `src/orchestrator/engine.rs`, `src/orchestrator/stack_secrets.rs`, `src/orchestrator/plan.rs`