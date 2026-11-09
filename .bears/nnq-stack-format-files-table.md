---
id: nnq
title: "Stack format: `files` table"
status: open
priority: P2
created: "2026-11-09T17:22:16.205452185Z"
updated: "2026-11-09T17:22:16.205452185Z"
tags:
  - stack
depends_on:
  - "5jg"
parent: xgf
---

## Summary
Parse and validate `[services.<svc>.files]` exactly as the M12 ADR specifies, carry it in the model, and include it in the config hash.

## Acceptance Criteria
- [ ] `Service.files: BTreeMap<ContainerPath, ConfigFile>`, where `ConfigFile` holds the source (`Inline(String)` or `Secret(name)`), `mode` and `owner` per the ADR. `ContainerPath` is a validated absolute path type (`FromStr` / `Display` / `AsRef<str>`) that is reusable by `tmpfs` (M13).
- [ ] Errors, all collected with spans: relative or unnormalized path, `/`, a duplicate, a path equal to or inside a volume mount path, an inline file over the size cap, a stack total over the cap, a bad secret name format, an invalid mode.
- [ ] `ConfigHash::new` includes inline content (or its hash) and secret refs with versions. A secret-backed file's version comes from the same `SecretVersions` the env secrets use.
- [ ] Engine pre-flight (`check_secrets` in `src/orchestrator/engine.rs`) also rejects missing file secrets by name.
- [ ] stack-format.md §2.2 documents `files` with an example and the UID / `mode` note. The schema is regenerated if zcu has landed.

## Testing
- `src/stack/tests.rs` snapshots for every rule; config-hash tests in `src/orchestrator/plan.rs` (content change → new hash, secret version change → new hash, unrelated service unchanged).
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## References
- The M12 ADR (task 5jg); ADR-0029
- `site/content/wiki/stack-format.md` §2.2
- `src/stack/raw.rs`, `src/stack/mod.rs`, `src/orchestrator/plan.rs`, `src/orchestrator/engine.rs`