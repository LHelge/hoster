---
id: "7ne"
title: "Log store: logs.db with its own pool, migrations and schema"
status: open
priority: P2
created: "2026-11-12T19:32:58.233864454Z"
updated: "2026-11-12T19:32:58.233864454Z"
tags:
  - store
  - observability
parent: f8z
---

## Summary
Create the second SQLite file of ADR-0067 §2: `logs.db` beside `hoster.db`, with its own pool, its own reversible migrations and the schema for container records, hoster's own records (ADR-0068) and the proxy's request rows. Later tasks in this epic write and read through the type this task introduces; nothing writes to it yet.

## Acceptance Criteria
- [ ] A `LogStore` type (in `src/store/` or its own module, whichever keeps the public API narrow) opens `<state_dir>/logs.db`: mode 0600, WAL, `auto_vacuum = INCREMENTAL` set before the first table is created, its own `SqlitePool`
- [ ] Its migrations live in their own directory (for example `migrations_logs/`) under their own `sqlx::migrate!` migrator, created with `sqlx migrate add -r --source <dir>`; every `.up.sql` has a `.down.sql` that restores the previous schema exactly, and the store tests that enforce this for `hoster.db` cover this migrator too (ADR-0052)
- [ ] `log_records` as ADR-0067 §4 with ADR-0068's amendment: ascending id, time (ms since the epoch), stack (nullable), service, generation (nullable), stream (`stdout`, `stderr`, `hoster`), level (nullable), message, fields (JSON text, nullable), cut flag, trace id and span id (nullable). Indexes on `(stack, time)` for a stack over a span and on `time` for the all-stacks scope of the Telemetry page
- [ ] `requests` as ADR-0067 §5: time, host, prefix, stack, service, method, path, status, ms to the response header, ms in total, response bytes, client address, upstream failed, request id. Index on `(stack, time)` and on `time`
- [ ] A `log_cursors` table: container id, the podman timestamp of the last stored line (ns), for the resume task
- [ ] No foreign keys and no reference from `hoster.db` (ADR-0067 §2)
- [ ] The supervisor opens it at startup beside `Store`; if it cannot be opened hoster still starts, logs the error and the rest of the log store stays off (the live container log page of ADR-0044 keeps working). `doctor` reports it in a later task
- [ ] `hoster backup db` does not touch it (ADR-0067 §9); a test says so

## Implementation Notes
- `src/store/mod.rs` shows how `Store` opens its pool (`SqliteConnectOptions`, journal mode, synchronous) and runs `MIGRATOR`; follow it, but keep `LogStore` a separate type so the two writers never share a pool
- `auto_vacuum` only takes effect on an empty file or after a full `VACUUM`; set it in the connect options or a `PRAGMA` before the migrator runs, and test that a fresh file reports `incremental`
- Record and row types (`LogRecord`, `RequestRow`, `Stream`, `Level`) belong here, with `FromStr`/`Display` for the text columns
- Times are integers (ms); trace and span ids are stored as lowercase hex text (32 and 16 chars) so they can be shown and linked without conversion

## Edge Cases
- A `logs.db` left by a newer hoster (unknown migration): refuse to open it, as `Store` does, rather than writing into it
- The state directory's permissions are those of `hoster.db`; a file created with a looser umask is chmod-ed

## Testing
- Store tests: fresh open, reopen, migrate up and down, `auto_vacuum` mode, file mode
- `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, tests pass

## References
- ADR-0067 (§2, §4, §5, §9), ADR-0068, ADR-0022, ADR-0052, ADR-0030
- `src/store/mod.rs`, `migrations/`, `src/supervisor.rs`