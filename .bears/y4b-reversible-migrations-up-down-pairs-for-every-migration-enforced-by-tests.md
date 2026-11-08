---
id: y4b
title: "Reversible migrations: up/down pairs for every migration, enforced by tests"
status: done
priority: P1
created: "2026-11-08T08:25:15.960144529Z"
updated: "2026-11-08T09:26:49.204984730Z"
tags:
  - store
parent: tbf
attempts: 1
---

## Summary
Convert every migration in `migrations/` to a sqlx reversible pair (`<version>_<name>.up.sql` / `.down.sql`) and add tests that make an irreversible or broken down migration fail CI. This is what lets the newer binary roll the schema back (ADR-0052).

## Acceptance Criteria
- [ ] 0001–0007 become `.up.sql` (content unchanged) + a `.down.sql` that restores the exact previous schema (tables, indexes, triggers, columns); no installs exist yet, so changed file names/descriptions are fine
- [ ] Test: every migration in `MIGRATOR.iter()` that is an up has a matching down with the same version, and there are no `Simple` migrations
- [ ] Test: on a fresh DB, run all ups → `MIGRATOR.undo(0)` → run all ups again; succeeds, and the schema after the round trip equals the schema after the first run (compare `sqlite_schema` rows)
- [ ] Test: for each version N, migrate to latest, undo to N-1, and assert the schema equals a fresh `run_to(N-1)` (so each down restores its predecessor exactly)
- [ ] `Store` exposes the schema version the binary expects (latest embedded migration version) and the version a database is at; both used later by `hoster version --json` and rollback
- [ ] CLAUDE.md or docs/architecture.md notes the rule: every new migration must be reversible (`sqlx migrate add -r`)

## Implementation Notes
- sqlx 0.9: `Migrator::undo(conn, target)` reverts only migrations that have a down file, newest first; `validate_applied_migrations` fails with `VersionMissing` when the DB has migrations the binary doesn't know.
- SQLite `ALTER TABLE DROP COLUMN` is available in the bundled SQLite; table rebuilds where needed.

## Testing
- The tests above; full suite green

## References
- ADR-0052 (Reversible migrations and rollback)
- ADR-0022
- src/store/mod.rs