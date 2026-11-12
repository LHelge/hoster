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
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};

pub use types::{
    Level, LogBatch, LogCursor, LogHead, LogRecord, RequestId, RequestRow, Stream, UnknownLogText,
};

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
    /// Rows per multi-row `INSERT`: at 14 columns, far under SQLite's 32766 bound parameters.
    const ROWS_PER_INSERT: usize = 500;

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

    /// Writes `batch` in one transaction and returns the newest ids after it. Rows are inserted
    /// in chunks of multi-row `INSERT`s, under SQLite's limit on bound parameters.
    pub async fn append(&self, batch: &LogBatch) -> Result<LogHead> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for chunk in batch.records.chunks(Self::ROWS_PER_INSERT) {
            let mut insert = QueryBuilder::<Sqlite>::new(
                "INSERT INTO log_records \
                 (time, stack, service, generation, stream, level, message, fields, cut, marker) ",
            );
            insert.push_values(chunk, |mut row, record| {
                row.push_bind(record.time.as_millis())
                    .push_bind(record.stack.as_deref())
                    .push_bind(record.service.as_str())
                    .push_bind(record.generation.map(i64::from))
                    .push_bind(record.stream.as_str())
                    .push_bind(record.level.map(Level::as_str))
                    .push_bind(record.message.as_str())
                    .push_bind(record.fields.as_deref())
                    .push_bind(record.cut)
                    .push_bind(record.marker);
            });
            insert.build().execute(&mut *tx).await?;
        }
        for chunk in batch.requests.chunks(Self::ROWS_PER_INSERT) {
            let mut insert = QueryBuilder::<Sqlite>::new(
                "INSERT INTO requests (time, host, prefix, stack, service, method, path, status, \
                 header_ms, total_ms, bytes, client, upstream_failed, request_id) ",
            );
            insert.push_values(chunk, |mut row, request| {
                row.push_bind(request.time.as_millis())
                    .push_bind(request.host.as_str())
                    .push_bind(request.prefix.as_str())
                    .push_bind(request.stack.as_str())
                    .push_bind(request.service.as_str())
                    .push_bind(request.method.as_str())
                    .push_bind(request.path.as_str())
                    .push_bind(i64::from(request.status))
                    .push_bind(request.to_header.map(Self::millis))
                    .push_bind(Self::millis(request.total))
                    .push_bind(i64::try_from(request.bytes).unwrap_or(i64::MAX))
                    .push_bind(request.client.to_string())
                    .push_bind(request.upstream_failed)
                    .push_bind(request.request_id.to_string());
            });
            insert.build().execute(&mut *tx).await?;
        }
        for chunk in batch.cursors.chunks(Self::ROWS_PER_INSERT) {
            let mut upsert =
                QueryBuilder::<Sqlite>::new("INSERT INTO log_cursors (container, time_ns) ");
            upsert.push_values(chunk, |mut row, cursor| {
                row.push_bind(cursor.container.as_str())
                    .push_bind(cursor.time_ns);
            });
            upsert.push(" ON CONFLICT (container) DO UPDATE SET time_ns = excluded.time_ns");
            upsert.build().execute(&mut *tx).await?;
        }
        let head = Self::head_in(&mut tx).await?;
        tx.commit().await?;
        Ok(head)
    }

    /// The newest committed ids.
    pub async fn head(&self) -> Result<LogHead> {
        let mut conn = self.pool.acquire().await?;
        Self::head_in(&mut conn).await
    }

    /// Where the follower of `container` got to, if it stored anything.
    pub async fn cursor(&self, container: &str) -> Result<Option<i64>> {
        Ok(
            sqlx::query_scalar("SELECT time_ns FROM log_cursors WHERE container = ?")
                .bind(container)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn head_in(conn: &mut SqliteConnection) -> Result<LogHead> {
        let (record, request): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT coalesce(max(id), 0) FROM log_records), \
                    (SELECT coalesce(max(id), 0) FROM requests)",
        )
        .fetch_one(conn)
        .await?;
        Ok(LogHead { record, request })
    }

    /// A duration as whole milliseconds, saturating.
    fn millis(duration: Duration) -> i64 {
        i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
    }

    /// The pool, for tests elsewhere in the crate that check what was written before the query
    /// API reads it.
    #[cfg(test)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Closes every connection, checkpointing the WAL.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
