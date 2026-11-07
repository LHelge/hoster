# ADR-0022: State store is SQLite, accessed through sqlx

- Status: Accepted (resolves OQ-01)
- Date: 2026-11-07

## Context

hoster must persist:

- stacks and their applied revisions
- the current generation and digests per service
- deployment history with state transitions, which is also the crash-recovery log
  (architecture §5.4)
- port allocations
- web UI sessions (ADR-0021), webhook replay state and other small items

The store must be transactional, because "set current generation" is the deploy commit point. It
must be local (ADR-0020) and easy to back up.

## Decision

Use **SQLite** through **sqlx** (sqlx 0.9.0 checked, `sqlite` + `runtime-tokio` + `migrate`
features).

- **Location.** One database file under hoster's state directory, in WAL mode, with
  `foreign_keys=ON` and a busy timeout.
- **Migrations** are embedded with `sqlx::migrate!()` and run at startup (architecture §4 step 2).
- **Queries** use sqlx's compile-time checked macros (`query!` / `query_as!`). The offline query
  metadata (`.sqlx/`, via `cargo sqlx prepare`) is committed, so CI builds without a live
  database.
- **Access** is through a `SqlitePool` behind a small `Store` API. The orchestrator actor remains
  the logical single writer of desired state.

## Alternatives considered

The project owner made this decision. These alternatives were listed in OQ-01:

- **rusqlite in a `spawn_blocking` actor**: synchronous API, so hoster would build its own async
  bridge. There are no compile-time checked queries.
- **redb** (pure-Rust KV): no SQL, so history queries and migrations would be hand-rolled.
- **JSON/TOML files**: no transactions across files, and easy to corrupt.
- **Podman labels as the only state**: too little information, and immutable after create.

## Consequences

- An async-native API fits the tokio orchestrator runtime directly.
- `libsqlite3-sys` is bundled by sqlx's sqlite driver: a C compile at build time, with no system
  library dependency at runtime.
- Developers changing queries must run `cargo sqlx prepare` (documented in the README). CI checks
  that `.sqlx/` is up to date (`cargo sqlx prepare --check`).
- Web sessions can use a SQLite-backed `tower-sessions` store. `tower-sessions-sqlx-store` exists
  (0.15.0), but its sqlx version compatibility must be checked against sqlx 0.9, or a small store
  implemented in-house (ADR-0031).
- Certificates and the ACME account stay as files (ADR-0016). Backups cover the DB file through
  `VACUUM INTO` or the SQLite online backup, plus the files (ADR-0030).
