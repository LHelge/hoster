//! The log store (ADR-0067, amended by ADR-0068): `<state_dir>/logs.db`, a second SQLite file
//! beside `hoster.db` holding what managed containers print, hoster's own events at info and
//! above, one row per proxied request, and the log records and spans containers export over
//! OTLP (ADR-0069 §6).
//!
//! [`LogStore`] owns its own WAL-mode pool and its own reversible migrations
//! (`migrations_logs/` at the crate root, ADR-0052), so log bursts never share a writer with deploy
//! state and `hoster backup db` (ADR-0030) never sees them. The file is created with
//! `auto_vacuum = INCREMENTAL`, so retention can give pages back to the file system. Nothing in
//! `hoster.db` refers to it, and it has no foreign keys.
//!
//! Its own figures (the used size, the rows of the last hour) feed the System page's log store
//! card, and [`LogStore::inspect_in`] lets `hoster doctor` read an existing file without
//! creating or migrating it.
//!
//! Its queries are runtime-checked (`sqlx::query`, not `query!`): sqlx's macros check against
//! one database per crate, which is `hoster.db`'s. Every query here is exercised by the tests.

mod health;
mod query;
mod retention;
mod types;

#[cfg(test)]
mod tests;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{
    SqliteAutoVacuum, SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions,
    SqliteSynchronous,
};
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};

pub use health::{LogFileFacts, RowCounts};
pub use query::{
    InvalidPattern, LevelCounts, LogEntry, LogPage, LogQuery, LogScope, LogTail, Page, PageAt,
    RecordKey, RequestCounts, RequestEntry, RequestQuery, RootQuery, ServiceSeen, StatusFilter,
    Tail, TextFilter,
};
pub use retention::{Removed, Retention, Sweep, SweepReport};
pub use types::{
    HexId, Level, LogBatch, LogCursor, LogHead, LogRecord, MicroTimestamp, RequestId, RequestRow,
    Span, SpanId, SpanKind, SpanStatus, Stream, TraceId, UnknownLogText,
};

use super::{Migrations, Result, SchemaVersion, Store, StoreError};

/// The embedded migrations from `migrations_logs/`, reversible like the state store's; the store
/// tests enforce it for both.
pub(super) static LOG_MIGRATOR: Migrator = sqlx::migrate!("./migrations_logs");

/// Handle to the log store. Cheap to clone; clones share the pools.
///
/// Writes (the recorder, retention) go through one pool and reads (the logs pages) through a
/// second, read-only one, so a page never queues behind a commit for a connection; in WAL mode
/// it reads the last committed state while the recorder writes.
#[derive(Debug, Clone)]
pub struct LogStore {
    pool: SqlitePool,
    reads: SqlitePool,
    path: PathBuf,
}

impl LogStore {
    /// File name of the database inside the state directory, beside [`Store::FILE_NAME`].
    pub const FILE_NAME: &str = "logs.db";
    /// How long a connection waits for a lock held by another one before failing.
    const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
    /// The recorder, and retention when it runs.
    const WRITE_CONNECTIONS: u32 = 2;
    /// Concurrent logs pages.
    const READ_CONNECTIONS: u32 = 4;
    /// Rows per multi-row `INSERT`: at 17 columns at most, far under SQLite's 32766 bound
    /// parameters.
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
            .max_connections(Self::WRITE_CONNECTIONS)
            .connect_with(options)
            .await
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?;

        Store::quick_check(&pool, &path).await?;
        LOG_MIGRATOR
            .run(&pool)
            .await
            .map_err(|source| StoreError::Migrate {
                path: path.clone(),
                source,
            })?;

        // Opened once the file is in WAL mode and migrated: a read-only connection can neither
        // change the journal mode nor create tables.
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .read_only(true)
            .busy_timeout(Self::BUSY_TIMEOUT);
        let reads = SqlitePoolOptions::new()
            .max_connections(Self::READ_CONNECTIONS)
            .connect_with(options)
            .await
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?;
        Ok(Self { pool, reads, path })
    }

    /// Opens the existing `logs.db` under `state_dir` without creating or migrating it, with
    /// one connection for both pools: for `hoster rollback` to read (`read_only`, next to a
    /// running hoster) and undo (with hoster stopped) its schema.
    async fn open_existing(state_dir: &Path, read_only: bool) -> Result<Self> {
        let path = state_dir.join(Self::FILE_NAME);
        if !path.is_file() {
            return Err(StoreError::Missing(path));
        }
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .busy_timeout(Self::BUSY_TIMEOUT);
        let options = if read_only {
            options.read_only(true)
        } else {
            options.journal_mode(SqliteJournalMode::Wal)
        };
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            reads: pool.clone(),
            pool,
            path,
        })
    }

    /// The log schema this binary migrates `logs.db` to: its newest embedded migration.
    pub fn expected_schema_version() -> SchemaVersion {
        Migrations(&LOG_MIGRATOR).expected()
    }

    /// How many of this binary's log migrations undoing the schema from `from` down to `to`
    /// reverts, like [`Store::migrations_between`].
    pub fn migrations_between(to: SchemaVersion, from: SchemaVersion) -> usize {
        Migrations(&LOG_MIGRATOR).between(to, from)
    }

    /// The log schema the file is at, read without migrating; `0` if nothing was applied.
    pub async fn schema_version(&self) -> Result<SchemaVersion> {
        Migrations::applied(&self.pool).await
    }

    /// The log schema of the existing `logs.db` under `state_dir`; `None` when there is none.
    pub async fn schema_version_in(state_dir: &Path) -> Result<Option<SchemaVersion>> {
        let store = match Self::open_existing(state_dir, true).await {
            Ok(store) => store,
            Err(StoreError::Missing(_)) => return Ok(None),
            Err(err) => return Err(err),
        };
        let schema = store.schema_version().await;
        store.close().await;
        schema.map(Some)
    }

    /// Undoes the log migrations of the existing `logs.db` under `state_dir` newer than
    /// `target`, with this binary's down scripts, and returns the schema the file is at
    /// afterwards (`hoster rollback`, ADR-0052), like [`Store::undo_migrations`]. Nothing else
    /// may have the file open. It is never created.
    pub async fn undo_migrations(state_dir: &Path, target: SchemaVersion) -> Result<SchemaVersion> {
        let store = Self::open_existing(state_dir, false).await?;
        let result = Migrations(&LOG_MIGRATOR)
            .undo(&store.pool, &store.path, target)
            .await;
        store.close().await;
        result
    }

    /// Moves `logs.db` under `state_dir`, with its `-wal`/`-shm` files, aside to
    /// `logs.db.<suffix>` (and its `-wal`/`-shm`), replacing an earlier file of that name, and
    /// returns the new path. Logs are expendable (ADR-0067 §9): when `hoster rollback` cannot
    /// undo the file, the previous binary starts on a new, empty one instead. Nothing may have
    /// the file open.
    pub fn move_aside(state_dir: &Path, suffix: &str) -> Result<PathBuf> {
        let path = state_dir.join(Self::FILE_NAME);
        let aside = state_dir.join(format!("{}.{suffix}", Self::FILE_NAME));
        let io_err = |path: &Path| {
            let path = path.to_owned();
            move |source| StoreError::Io { path, source }
        };
        // The sidecars first: a `-wal` left behind would be applied to the next file.
        for side in ["-wal", "-shm"] {
            let from = Self::sidecar(&path, side);
            let to = Self::sidecar(&aside, side);
            match fs::rename(&from, &to) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::remove_file(&to) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    other => other.map_err(io_err(&to))?,
                },
                other => other.map_err(io_err(&from))?,
            }
        }
        fs::rename(&path, &aside).map_err(io_err(&path))?;
        fs::File::open(state_dir)
            .and_then(|d| d.sync_all())
            .map_err(io_err(state_dir))?;
        Ok(aside)
    }

    /// `path` with `suffix` (`-wal`, `-shm`) appended to its file name.
    fn sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut side = path.as_os_str().to_owned();
        side.push(suffix);
        PathBuf::from(side)
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
                 (time, stack, service, generation, stream, level, message, fields, cut, marker, \
                 trace_id, span_id, received) ",
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
                    .push_bind(record.marker)
                    .push_bind(record.trace_id.map(|id| id.to_string()))
                    .push_bind(record.span_id.map(|id| id.to_string()))
                    .push_bind(record.received);
            });
            insert.build().execute(&mut *tx).await?;
        }
        for chunk in batch.requests.chunks(Self::ROWS_PER_INSERT) {
            let mut insert = QueryBuilder::<Sqlite>::new(
                "INSERT INTO requests (time, host, prefix, stack, service, method, path, status, \
                 header_ms, total_ms, bytes, client, upstream_failed, request_id, server_span_id, \
                 client_span_id, upstream_start) ",
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
                    .push_bind(request.request_id.to_string())
                    .push_bind(request.server_span.map(|id| id.to_string()))
                    .push_bind(request.client_span.map(|id| id.to_string()))
                    .push_bind(request.upstream_start);
            });
            insert.build().execute(&mut *tx).await?;
        }
        for chunk in batch.spans.chunks(Self::ROWS_PER_INSERT) {
            let mut insert = QueryBuilder::<Sqlite>::new(
                "INSERT INTO spans (trace_id, span_id, parent_span_id, stack, service, name, kind, \
                 start, duration, status, status_message, attributes, resource, events, received) ",
            );
            insert.push_values(chunk, |mut row, span| {
                row.push_bind(span.trace_id.to_string())
                    .push_bind(span.span_id.to_string())
                    .push_bind(span.parent.map(|id| id.to_string()))
                    .push_bind(span.stack.as_str())
                    .push_bind(span.service.as_str())
                    .push_bind(span.name.as_str())
                    .push_bind(span.kind.as_str())
                    .push_bind(span.start)
                    .push_bind(Self::micros(span.duration))
                    .push_bind(span.status.as_str())
                    .push_bind(span.status_message.as_deref())
                    .push_bind(span.attributes.as_deref())
                    .push_bind(span.resource.as_deref())
                    .push_bind(span.events.as_deref())
                    .push_bind(span.received);
            });
            insert.build().execute(&mut *tx).await?;
        }
        for chunk in batch.cursors.chunks(Self::ROWS_PER_INSERT) {
            let mut upsert =
                QueryBuilder::<Sqlite>::new("INSERT INTO log_cursors (container, time_ns, nth) ");
            upsert.push_values(chunk, |mut row, cursor| {
                row.push_bind(cursor.container.as_str())
                    .push_bind(cursor.time_ns)
                    .push_bind(cursor.nth);
            });
            upsert.push(
                " ON CONFLICT (container) DO UPDATE SET time_ns = excluded.time_ns, nth = excluded.nth",
            );
            upsert.build().execute(&mut *tx).await?;
        }
        for chunk in batch.forgotten.chunks(Self::ROWS_PER_INSERT) {
            let mut delete =
                QueryBuilder::<Sqlite>::new("DELETE FROM log_cursors WHERE container IN (");
            let mut ids = delete.separated(", ");
            for container in chunk {
                ids.push_bind(container.as_str());
            }
            delete.push(")");
            delete.build().execute(&mut *tx).await?;
        }
        let head = Self::head_in(&mut tx).await?;
        tx.commit().await?;
        Ok(head)
    }

    /// The newest committed ids.
    pub async fn head(&self) -> Result<LogHead> {
        let mut conn = self.reads.acquire().await?;
        Self::head_in(&mut conn).await
    }

    /// Where the follower of `container` got to, if it stored anything.
    pub async fn cursor(&self, container: &str) -> Result<Option<LogCursor>> {
        let row: Option<(i64, i64)> =
            sqlx::query_as("SELECT time_ns, nth FROM log_cursors WHERE container = ?")
                .bind(container)
                .fetch_optional(&self.reads)
                .await?;
        Ok(row.map(|(time_ns, nth)| LogCursor {
            container: container.to_owned(),
            time_ns,
            nth,
        }))
    }

    /// Every stored cursor, by container: where each follower resumes after a restart.
    pub async fn cursors(&self) -> Result<Vec<LogCursor>> {
        let rows: Vec<(String, i64, i64)> =
            sqlx::query_as("SELECT container, time_ns, nth FROM log_cursors ORDER BY container")
                .fetch_all(&self.reads)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(container, time_ns, nth)| LogCursor {
                container,
                time_ns,
                nth,
            })
            .collect())
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
    pub(super) fn millis(duration: Duration) -> i64 {
        i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
    }

    /// A duration as whole microseconds, saturating.
    pub(super) fn micros(duration: Duration) -> i64 {
        i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
    }

    /// The pool, for tests elsewhere in the crate that check what was written before the query
    /// API reads it.
    #[cfg(test)]
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Closes every connection, checkpointing the WAL.
    pub async fn close(&self) {
        self.reads.close().await;
        self.pool.close().await;
    }
}
