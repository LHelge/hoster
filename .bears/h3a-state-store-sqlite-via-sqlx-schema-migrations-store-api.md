---
id: h3a
title: "State store: SQLite via sqlx — schema, migrations, Store API"
status: done
priority: P1
created: "2026-11-07T14:45:17.181698219Z"
updated: "2026-11-07T22:38:13.388313501Z"
tags:
  - store
depends_on:
  - "26p"
parent: "5ch"
attempts: 1
---

## Summary
Implement the persistent state store per ADR-0022: SQLite through sqlx with embedded migrations and compile-time checked queries.

## Acceptance Criteria
- [ ] Schema with migrations for: stacks, stack_revisions (raw TOML + hash), services (current generation, digest), deployments (id, stack, target, state, source, timestamps), deployment_events (transition log), port_allocations, kv for small items
- [ ] sqlx 0.9 with features `sqlite`, `runtime-tokio`, `migrate`, `macros`; `SqlitePool` with WAL journal mode, foreign_keys=ON, busy_timeout; migrations in `migrations/` run via `sqlx::migrate!()` at startup
- [ ] Queries use `query!`/`query_as!`; offline metadata in `.sqlx/` committed; CI runs `cargo sqlx prepare --check` (README documents `cargo sqlx prepare`)
- [ ] Async `Store` API (typed methods, no SQL outside `store/`) used by the orchestrator actor
- [ ] 'Set current generation' is a single transaction (the deploy commit point, architecture §5.4)
- [ ] Data lives under the configured state dir with 0600/0700 permissions
- [ ] Startup step 2 wired in the supervisor

## Implementation Notes
- Keep the orchestrator actor the logical single writer of desired state; the pool still serializes SQLite writes
- Online backup helper (`VACUUM INTO`) for ADR-0030 can be a small follow-up

## Edge Cases & Considerations
- Corrupt/locked DB → fail startup with clear error
- Migration from an older schema version

## Testing
- Unit tests against a temp dir: CRUD, transaction rollback on error, migration idempotence
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` pass

## References
- ADR-0022
- docs/architecture.md §5.4