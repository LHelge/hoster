//! The log store (ADR-0067, amended by ADR-0068): `<state_dir>/logs.db`, a second SQLite file
//! beside `hoster.db` holding what managed containers print, hoster's own events at info and
//! above, and one row per proxied request.
//!
//! [`LogStore`] owns its own WAL-mode pool and its own reversible migrations
//! (`migrations_logs/` at the crate root, ADR-0052), so log bursts never share a writer with deploy
//! state and `hoster backup db` (ADR-0030) never sees them. The file is created with
//! `auto_vacuum = INCREMENTAL`, so retention can give pages back to the file system. Nothing in
//! `hoster.db` refers to it, and it has no foreign keys.
//!
//! Its queries are runtime-checked (`sqlx::query`, not `query!`): sqlx's macros check against
//! one database per crate, which is `hoster.db`'s. Every query here is exercised by the tests.

#![allow(
    dead_code,
    reason = "written by the recorder and read by the logs UI as they land (epic f8z)"
)]

mod types;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{
    SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions,
    SqliteSynchronous,
};

pub use types::{Level, LogRecord, RequestId, RequestRow, Stream, UnknownLogText};

use super::{Result, Store, StoreError};

/// The embedded migrations from `migrations_logs/`, reversible like the state store's; the store
/// tests enforce it for both.
pub(super) static LOG_MIGRATOR: Migrator = sqlx::migrate!("./migrations_logs");

/// Handle to the log store. Cheap to clone; clones share the pool.
#[derive(Debug, Clone)]
pub struct LogStore {
    pool: SqlitePool,
    path: PathBuf,
}

impl LogStore {
    /// File name of the database inside the state directory, beside [`Store::FILE_NAME`].
    pub const FILE_NAME: &str = "logs.db";
    /// How long a connection waits for a lock held by another one before failing.
    const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
    /// One writer (the recorder) and readers for the logs pages.
    const MAX_CONNECTIONS: u32 = 4;

    /// Opens (creating if needed) `logs.db` under `state_dir` and migrates it to the current
    /// schema. The directory and file get the state store's modes (0700 and 0600). A file
    /// migrated by a newer hoster is refused rather than written into.
    pub async fn open(state_dir: &Path) -> Result<Self> {
        Store::prepare_dir(state_dir)?;
        let path = state_dir.join(Self::FILE_NAME);
        Store::prepare_file(&path)?;

        // `auto_vacuum` only takes effect on a file without tables: here, the empty file
        // `prepare_file` created, before the migrations add any (the tests check the mode).
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .auto_vacuum(SqliteAutoVacuum::Incremental)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Self::BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(Self::MAX_CONNECTIONS)
            .connect_with(options)
            .await
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?;

        let store = Self { pool, path };
        Store::quick_check(&store.pool, &store.path).await?;
        LOG_MIGRATOR
            .run(&store.pool)
            .await
            .map_err(|source| StoreError::Migrate {
                path: store.path.clone(),
                source,
            })?;
        Ok(store)
    }

    /// Path of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Closes every connection, checkpointing the WAL.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
