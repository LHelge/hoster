//! State store: SQLite via sqlx (ADR-0022).
//!
//! This module is the only place in hoster that talks SQL. [`Store`] owns a WAL-mode
//! `SqlitePool` on `<state_dir>/hoster.db`, runs the embedded migrations (`migrations/` at the
//! crate root) when it opens, and exposes typed async methods. The queries are compile-time
//! checked against the offline metadata in `.sqlx/`; after changing one, run `cargo sqlx prepare`
//! (see README). [`LogStore`] is the second database, `logs.db` (ADR-0067), with its own pool and
//! migrations (`migrations_logs/`).
//!
//! The orchestrator is the logical single writer of desired state (ADR-0050); the pool
//! serializes SQLite writes underneath. [`Store::commit`] is the deploy commit point
//! (docs/architecture.md §5.4): it writes every per-service pointer of a deployment in one
//! transaction.

#![allow(
    dead_code,
    reason = "the Store API is consumed by the orchestrator and web UI"
)]

mod events;
mod images;
mod instance;
mod logs;
mod notifications;
mod secrets;
mod sessions;
mod traffic;
mod types;
mod updates;
mod webhook;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::migrate::{MigrateError, Migrator};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};
use thiserror::Error;

#[allow(
    unused_imports,
    reason = "the record types are written by the log store's recorder as it lands"
)]
pub use logs::{Level, LogRecord, LogStore, RequestId, RequestRow, Stream, UnknownLogText};
pub use notifications::{
    DeliveryOutcome, LastDelivery, NotificationSettings, NotifiedProblem, NotifierStep, ProblemKey,
};
pub use traffic::{Resolution, TrafficBucket, TrafficCounts, TrafficSeries};
#[allow(
    unused_imports,
    reason = "part of the Store API for its future callers"
)]
pub use types::{
    ContentHash, Deployment, DeploymentEvent, DeploymentId, DeploymentOutcome, DeploymentSource,
    DeploymentState, DigestSet, Event, EventCursor, EventId, EventKind, EventPage, EventQuery,
    ImageUse, ImageUseKind, InvalidEventCursor, NewDeployment, NewEvent, PortAllocation, PortMap,
    PortRequest, Provenance, RecordedImage, RevisionId, SchemaVersion, SealedSecret, SecretEntry,
    ServicePointer, SessionRecord, Severity, ShortDigest, ShortSha, Stack, StackRevision,
    Timestamp, UnknownEventText, WebhookRejection,
};
#[allow(
    unused_imports,
    reason = "used by the notifier and Settings → Notifications as they land"
)]
pub use updates::{
    Attempt, Candidate, CheckInterval, ImageCheck, ServiceUpdates, StoredCandidate, UpdateKind,
    UpdateSettings,
};

/// The embedded migrations from `migrations/`. Every one is a reversible `.up.sql` / `.down.sql`
/// pair (ADR-0052), so a newer binary can undo its schema back to an older binary's; the store
/// tests enforce this.
static MIGRATOR: Migrator = sqlx::migrate!();

/// Errors from the state store.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot prepare {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "cannot open the state database {} (is it corrupt, or locked by another hoster process?)",
        path.display()
    )]
    Open {
        path: PathBuf,
        #[source]
        source: sqlx::Error,
    },
    #[error("the state database {} is corrupt: {report}", path.display())]
    Corrupt { path: PathBuf, report: String },
    #[error("cannot migrate the state database {}", path.display())]
    Migrate {
        path: PathBuf,
        #[source]
        source: MigrateError,
    },
    #[error("no state database at {} (is state_dir right?)", .0.display())]
    Missing(PathBuf),
    #[error("{} already exists; choose a new snapshot file", .0.display())]
    SnapshotExists(PathBuf),
    #[error("the snapshot path {} is not valid UTF-8", .0.display())]
    SnapshotPath(PathBuf),
    #[error("cannot write a snapshot to {}", path.display())]
    Snapshot {
        path: PathBuf,
        #[source]
        source: sqlx::Error,
    },
    #[error("stack {0:?} does not exist")]
    UnknownStack(String),
    #[error("deployment {0} does not exist")]
    UnknownDeployment(DeploymentId),
    #[error("port {0} is already allocated")]
    PortTaken(u16),
    #[error("notifications need a from address and a recipient before they can be enabled")]
    NotificationsIncomplete,
    #[error("the traffic bucket at {} has a malformed latency histogram", start.as_millis())]
    Latency {
        start: Timestamp,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("state database error")]
    Database(#[from] sqlx::Error),
}

type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Handle to the state store. Cheap to clone; clones share the pool.
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
    path: PathBuf,
}

impl Store {
    /// File name of the database inside the state directory (docs/architecture.md §7.4).
    pub const FILE_NAME: &str = "hoster.db";
    /// How long a connection waits for a lock held by another one before failing.
    const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_CONNECTIONS: u32 = 4;

    /// Opens (creating if needed) the database under `state_dir` and migrates it to the current
    /// schema. Startup step 2 (docs/architecture.md §4).
    ///
    /// The state directory is made 0700 and the database file 0600; SQLite gives its `-wal` and
    /// `-shm` files the database file's mode.
    pub async fn open(state_dir: &Path) -> Result<Self> {
        Self::prepare_dir(state_dir)?;
        let path = state_dir.join(Self::FILE_NAME);
        Self::prepare_file(&path)?;

        let options = SqliteConnectOptions::new()
            .filename(&path)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
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
        store.check_integrity().await?;
        MIGRATOR
            .run(&store.pool)
            .await
            .map_err(|source| StoreError::Migrate {
                path: store.path.clone(),
                source,
            })?;
        Ok(store)
    }

    /// Opens the existing database under `state_dir` read-only, without creating or migrating
    /// it, for [`Store::snapshot`] next to a running hoster.
    pub async fn open_read_only(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(Self::FILE_NAME);
        if !path.is_file() {
            return Err(StoreError::Missing(path));
        }
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .read_only(true)
            .busy_timeout(Self::BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?;
        Ok(Self { pool, path })
    }

    /// Opens the existing database under `state_dir` for writing, without creating or
    /// migrating it: for `hoster update` and `hoster rollback` to record their event
    /// (ADR-0055) in a database the running hoster, or a stopped one, owns.
    pub async fn open_existing(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join(Self::FILE_NAME);
        if !path.is_file() {
            return Err(StoreError::Missing(path));
        }
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .busy_timeout(Self::BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?;
        Ok(Self { pool, path })
    }

    /// Writes a consistent snapshot of the database to `dest` with `VACUUM INTO` (ADR-0030).
    /// Safe while hoster runs: the snapshot is one read transaction, so it sees every committed
    /// write before it and none after.
    ///
    /// The snapshot is written 0600 to a temporary file next to `dest`, checked with
    /// `PRAGMA integrity_check` and then moved into place, so `dest` only ever appears
    /// complete. An existing `dest` is never overwritten, even one created while the snapshot
    /// is being written.
    pub async fn snapshot(&self, dest: &Path) -> Result<()> {
        if fs::symlink_metadata(dest).is_ok() {
            return Err(StoreError::SnapshotExists(dest.to_owned()));
        }
        let tmp = Self::snapshot_tmp_path(dest).map_err(|source| StoreError::Io {
            path: dest.to_owned(),
            source,
        })?;
        let tmp_str = tmp
            .to_str()
            .ok_or_else(|| StoreError::SnapshotPath(dest.to_owned()))?
            .to_owned();
        // `VACUUM INTO` accepts an empty file; creating it first fixes its mode at 0600.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|source| StoreError::Io {
                path: tmp.clone(),
                source,
            })?;
        let result = self.write_snapshot(&tmp, &tmp_str, dest).await;
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    async fn write_snapshot(&self, tmp: &Path, tmp_str: &str, dest: &Path) -> Result<()> {
        let snapshot_err = |source| StoreError::Snapshot {
            path: dest.to_owned(),
            source,
        };
        sqlx::query("VACUUM INTO ?")
            .bind(tmp_str)
            .execute(&self.pool)
            .await
            .map_err(snapshot_err)?;
        Self::verify_snapshot(tmp, dest).await?;
        let io_err = |source| StoreError::Io {
            path: dest.to_owned(),
            source,
        };
        OpenOptions::new()
            .read(true)
            .open(tmp)
            .and_then(|f| f.sync_all())
            .map_err(io_err)?;
        Self::publish_snapshot(tmp, dest)?;
        // Persist the rename. `parent()` of a bare file name is "", meaning the current dir.
        let dir = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(io_err)
    }

    /// Runs `PRAGMA integrity_check` on a freshly written snapshot.
    async fn verify_snapshot(tmp: &Path, dest: &Path) -> Result<()> {
        let options = SqliteConnectOptions::new().filename(tmp).read_only(true);
        let mut conn = sqlx::ConnectOptions::connect(&options)
            .await
            .map_err(|source| StoreError::Snapshot {
                path: dest.to_owned(),
                source,
            })?;
        let report: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_all(&mut conn)
            .await
            .map_err(|source| StoreError::Snapshot {
                path: dest.to_owned(),
                source,
            })?;
        let _ = sqlx::Connection::close(conn).await;
        match report.as_slice() {
            [ok] if ok == "ok" => Ok(()),
            _ => Err(StoreError::Corrupt {
                path: dest.to_owned(),
                report: report.join("; "),
            }),
        }
    }

    /// Moves the finished snapshot `tmp` to `dest` without ever replacing an existing `dest`.
    ///
    /// `rename` silently replaces its target, so the snapshot is hard-linked into place (which
    /// fails atomically if `dest` exists) and the temporary name removed. Only on a filesystem
    /// without hard links does it fall back to check-then-rename.
    fn publish_snapshot(tmp: &Path, dest: &Path) -> Result<()> {
        let io_err = |source| StoreError::Io {
            path: dest.to_owned(),
            source,
        };
        match fs::hard_link(tmp, dest) {
            Ok(()) => {
                // `dest` is complete; a leftover hidden temp name is not worth failing over.
                let _ = fs::remove_file(tmp);
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                Err(StoreError::SnapshotExists(dest.to_owned()))
            }
            Err(_) if fs::symlink_metadata(dest).is_ok() => {
                Err(StoreError::SnapshotExists(dest.to_owned()))
            }
            Err(_) => fs::rename(tmp, dest).map_err(io_err),
        }
    }

    /// `<dir>/.<name>.tmp-<pid>`: hidden, in the same directory so moving it into place is
    /// atomic. The path is absolute: with URI filenames enabled (as sqlx does), SQLite would
    /// parse a relative path beginning with `file:` as a URI in `VACUUM INTO`.
    fn snapshot_tmp_path(dest: &Path) -> io::Result<PathBuf> {
        let dest = std::path::absolute(dest)?;
        let mut name = std::ffi::OsString::from(".");
        name.push(dest.file_name().unwrap_or_else(|| "snapshot".as_ref()));
        name.push(format!(".tmp-{}", std::process::id()));
        Ok(dest.with_file_name(name))
    }

    /// The schema version this binary migrates databases to: its newest embedded migration.
    pub fn expected_schema_version() -> SchemaVersion {
        MIGRATOR
            .iter()
            .map(|m| m.version)
            .max()
            .unwrap_or_default()
            .into()
    }

    /// The schema version of this binary's migration named `description` (`events` for
    /// `0008_events`), so tests can target a schema relative to it rather than to the newest.
    #[cfg(test)]
    pub(crate) fn migration_version(description: &str) -> SchemaVersion {
        MIGRATOR
            .iter()
            .find(|m| m.migration_type.is_up_migration() && m.description == description)
            .map(|m| m.version)
            .unwrap_or_else(|| panic!("no migration named {description:?}"))
            .into()
    }

    /// How many of this binary's migrations undoing the schema from `from` down to `to` reverts:
    /// those newer than `to` and no newer than `from` (what `hoster rollback` warns about).
    pub fn migrations_between(to: SchemaVersion, from: SchemaVersion) -> usize {
        let (to, from) = (i64::from(to), i64::from(from));
        MIGRATOR
            .iter()
            .filter(|m| m.migration_type.is_up_migration() && m.version > to && m.version <= from)
            .count()
    }

    /// The schema version the database is at: its newest successfully applied migration, `0`
    /// if none has been applied. Reads without migrating, so it also works on a store from
    /// [`Store::open_read_only`] whose database a newer or older binary migrated.
    pub async fn schema_version(&self) -> Result<SchemaVersion> {
        let has_table: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM sqlite_schema \
             WHERE type = 'table' AND name = '_sqlx_migrations')",
        )
        .fetch_one(&self.pool)
        .await?;
        if !has_table {
            return Ok(SchemaVersion::default());
        }
        let version: Option<i64> =
            sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
                .fetch_one(&self.pool)
                .await?;
        Ok(version.unwrap_or_default().into())
    }

    /// Undoes the migrations of the existing database under `state_dir` newer than `target`,
    /// with this binary's down scripts, and returns the schema version the database is at
    /// afterwards (`hoster rollback`, ADR-0052). Nothing else may have the database open: the
    /// caller stops the service first. The database is never created.
    ///
    /// Each down script runs in its own transaction, so a failure leaves the database at the
    /// last migration that was undone; the caller restores its snapshot.
    pub async fn undo_migrations(state_dir: &Path, target: SchemaVersion) -> Result<SchemaVersion> {
        let store = Self::open_existing(state_dir).await?;
        let result = async {
            store.check_integrity().await?;
            MIGRATOR
                .undo(&store.pool, target.into())
                .await
                .map_err(|source| StoreError::Migrate {
                    path: store.path.clone(),
                    source,
                })?;
            store.schema_version().await
        }
        .await;
        store.close().await;
        result
    }

    /// Replaces the database under `state_dir` with the snapshot at `snapshot` (written by
    /// [`Store::snapshot`]). Nothing may have the database open. The snapshot is copied to a
    /// temporary file next to the database, fsynced and renamed over it; the old `-wal`/`-shm`
    /// files are removed first so SQLite never applies a stale log to the restored file. The
    /// snapshot itself is kept.
    pub fn restore(state_dir: &Path, snapshot: &Path) -> Result<()> {
        let path = state_dir.join(Self::FILE_NAME);
        let io_err = |path: &Path| {
            let path = path.to_owned();
            move |source| StoreError::Io { path, source }
        };
        let tmp = Self::snapshot_tmp_path(&path).map_err(io_err(&path))?;
        let copied = (|| {
            let mut from = fs::File::open(snapshot).map_err(io_err(snapshot))?;
            let mut to = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(io_err(&tmp))?;
            io::copy(&mut from, &mut to).map_err(io_err(&tmp))?;
            to.sync_all().map_err(io_err(&tmp))
        })();
        if let Err(err) = copied {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        for suffix in ["-wal", "-shm"] {
            let mut side = path.as_os_str().to_owned();
            side.push(suffix);
            let side = PathBuf::from(side);
            match fs::remove_file(&side) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                other => other.map_err(io_err(&side))?,
            }
        }
        if let Err(source) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(StoreError::Io { path, source });
        }
        fs::File::open(state_dir)
            .and_then(|d| d.sync_all())
            .map_err(io_err(state_dir))
    }

    /// Path of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Closes every connection, checkpointing the WAL.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    fn prepare_dir(dir: &Path) -> Result<()> {
        let io_err = |source| StoreError::Io {
            path: dir.to_owned(),
            source,
        };
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_err)?;
        fs::set_permissions(dir, Permissions::from_mode(0o700)).map_err(io_err)
    }

    /// Creates the database file 0600 if it is missing and tightens it, and any `-wal`/`-shm`
    /// file left from an earlier run, to 0600. SQLite only sets a mode when it creates a file.
    fn prepare_file(path: &Path) -> Result<()> {
        let io_err = |source| StoreError::Io {
            path: path.to_owned(),
            source,
        };
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map_err(io_err)?;
        fs::set_permissions(path, Permissions::from_mode(0o600)).map_err(io_err)?;
        for suffix in ["-wal", "-shm"] {
            let mut side = path.as_os_str().to_owned();
            side.push(suffix);
            let side = PathBuf::from(side);
            match fs::set_permissions(&side, Permissions::from_mode(0o600)) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                other => other.map_err(|source| StoreError::Io { path: side, source })?,
            }
        }
        Ok(())
    }

    async fn check_integrity(&self) -> Result<()> {
        Self::quick_check(&self.pool, &self.path).await
    }

    /// `PRAGMA quick_check` on the database at `path` behind `pool`; shared with [`LogStore`].
    async fn quick_check(pool: &SqlitePool, path: &Path) -> Result<()> {
        // PRAGMAs cannot be described by the query macros.
        let report: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
            .fetch_all(pool)
            .await
            .map_err(|source| StoreError::Open {
                path: path.to_owned(),
                source,
            })?;
        match report.as_slice() {
            [ok] if ok == "ok" => Ok(()),
            _ => Err(StoreError::Corrupt {
                path: path.to_owned(),
                report: report.join("; "),
            }),
        }
    }

    /// Starts a write transaction. `BEGIN IMMEDIATE` takes the write lock up front (waiting up to
    /// the busy timeout for it): a deferred transaction that reads first and then writes cannot
    /// upgrade its lock once another connection has committed, and SQLite fails it at once with
    /// `SQLITE_BUSY` instead of waiting.
    async fn begin(&self) -> Result<Transaction<'static, Sqlite>> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    // ---------------------------------------------------------------------------------------
    // Stacks and revisions
    // ---------------------------------------------------------------------------------------

    /// Records an applied stack file as the stack's next revision, creating the stack if it is
    /// new. The revision pins no images; see [`Store::add_pinned_revision`].
    pub async fn add_revision(&self, stack: &str, toml: &str) -> Result<StackRevision> {
        self.add_pinned_revision(stack, toml, &DigestSet::new())
            .await
    }

    /// Records an applied stack file as the stack's next revision together with the digests its
    /// third-party images were resolved to (ADR-0025), creating the stack if it is new.
    pub async fn add_pinned_revision(
        &self,
        stack: &str,
        toml: &str,
        images: &DigestSet,
    ) -> Result<StackRevision> {
        let mut tx = self.begin().await?;
        let revision =
            Self::add_pinned_revision_in(&mut tx, stack, toml, images, Timestamp::now()).await?;
        tx.commit().await?;
        Ok(revision)
    }

    /// [`add_pinned_revision`](Self::add_pinned_revision), recording the event `event` builds
    /// from the new revision in the event log (ADR-0055) in the same transaction.
    pub async fn add_pinned_revision_recorded(
        &self,
        stack: &str,
        toml: &str,
        images: &DigestSet,
        event: impl FnOnce(&StackRevision) -> NewEvent,
    ) -> Result<(StackRevision, Event)> {
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        let revision = Self::add_pinned_revision_in(&mut tx, stack, toml, images, now).await?;
        let recorded = Self::insert_event(&mut tx, &event(&revision), now).await?;
        tx.commit().await?;
        Ok((revision, recorded))
    }

    /// The revision insert on `conn`, inside the caller's transaction.
    async fn add_pinned_revision_in(
        conn: &mut SqliteConnection,
        stack: &str,
        toml: &str,
        images: &DigestSet,
        now: Timestamp,
    ) -> Result<StackRevision> {
        let hash = ContentHash::of(toml);
        sqlx::query!(
            "INSERT INTO stacks (name, created_at, updated_at) VALUES (?1, ?2, ?2)
             ON CONFLICT (name) DO UPDATE SET updated_at = excluded.updated_at, removed_at = NULL",
            stack,
            now,
        )
        .execute(&mut *conn)
        .await?;
        let revision = sqlx::query_as!(
            StackRevision,
            r#"INSERT INTO stack_revisions (stack, revision, toml, hash, created_at)
               VALUES (?1,
                       (SELECT COALESCE(MAX(revision), 0) + 1 FROM stack_revisions WHERE stack = ?1),
                       ?2, ?3, ?4)
               RETURNING id AS "id!: RevisionId", stack AS "stack!", revision AS "revision!: u32",
                         toml AS "toml!", hash AS "hash!: ContentHash",
                         created_at AS "created_at!: Timestamp""#,
            stack,
            toml,
            hash,
            now,
        )
        .fetch_one(&mut *conn)
        .await?;
        for (service, digest) in images {
            sqlx::query!(
                "INSERT INTO revision_images (revision_id, service, digest) VALUES (?, ?, ?)",
                revision.id,
                service,
                digest,
            )
            .execute(&mut *conn)
            .await?;
        }
        Ok(revision)
    }

    /// The image digests a revision pins, by service (empty for an unknown revision).
    pub async fn revision_images(&self, id: RevisionId) -> Result<DigestSet> {
        Ok(sqlx::query!(
            "SELECT service, digest FROM revision_images WHERE revision_id = ?",
            id,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(|row| (row.service, row.digest))
        .collect())
    }

    /// One stack, if it exists and has not been removed.
    pub async fn stack(&self, name: &str) -> Result<Option<Stack>> {
        Ok(sqlx::query_as!(
            Stack,
            r#"SELECT name, last_generation AS "last_generation: u32",
                      created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
               FROM stacks WHERE name = ? AND removed_at IS NULL"#,
            name,
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Every stack that has not been removed, by name.
    pub async fn stacks(&self) -> Result<Vec<Stack>> {
        Ok(sqlx::query_as!(
            Stack,
            r#"SELECT name, last_generation AS "last_generation: u32",
                      created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp"
               FROM stacks WHERE removed_at IS NULL ORDER BY name"#,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Removes a stack: deletes its service pointers and port reservations and marks it
    /// removed. Its revisions and deployment history are kept; applying the stack again revives
    /// it. Returns whether a stack of that name existed and was not removed yet.
    pub async fn remove_stack(&self, name: &str) -> Result<bool> {
        let mut tx = self.begin().await?;
        let removed = Self::remove_stack_in(&mut tx, name, Timestamp::now()).await?;
        tx.commit().await?;
        Ok(removed)
    }

    /// [`remove_stack`](Self::remove_stack), recording `event` in the event log (ADR-0055) in
    /// the same transaction: the event is stored if and only if the stack was removed. `None`
    /// when there was no such stack.
    pub async fn remove_stack_recorded(
        &self,
        name: &str,
        event: &NewEvent,
    ) -> Result<Option<Event>> {
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        if !Self::remove_stack_in(&mut tx, name, now).await? {
            return Ok(None);
        }
        let recorded = Self::insert_event(&mut tx, event, now).await?;
        tx.commit().await?;
        Ok(Some(recorded))
    }

    /// The removal on `conn`, inside the caller's transaction.
    async fn remove_stack_in(
        conn: &mut SqliteConnection,
        name: &str,
        now: Timestamp,
    ) -> Result<bool> {
        let done = sqlx::query!(
            "UPDATE stacks SET removed_at = ?1, updated_at = ?1
             WHERE name = ?2 AND removed_at IS NULL",
            now,
            name,
        )
        .execute(&mut *conn)
        .await?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query!("DELETE FROM services WHERE stack = ?", name)
            .execute(&mut *conn)
            .await?;
        sqlx::query!("DELETE FROM port_allocations WHERE stack = ?", name)
            .execute(&mut *conn)
            .await?;
        // What the update checker found goes too (ADR-0061 §8); its updates cascade.
        sqlx::query!("DELETE FROM image_checks WHERE stack = ?", name)
            .execute(&mut *conn)
            .await?;
        sqlx::query!("DELETE FROM auto_update_attempts WHERE stack = ?", name)
            .execute(&mut *conn)
            .await?;
        Ok(true)
    }

    /// One revision by id.
    pub async fn revision(&self, id: RevisionId) -> Result<Option<StackRevision>> {
        Ok(sqlx::query_as!(
            StackRevision,
            r#"SELECT id AS "id: RevisionId", stack, revision AS "revision: u32", toml,
                      hash AS "hash: ContentHash", created_at AS "created_at: Timestamp"
               FROM stack_revisions WHERE id = ?"#,
            id,
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// The newest revision of a stack: its active one. A removed stack has none, although
    /// [`Store::revisions`] still lists its history.
    pub async fn latest_revision(&self, stack: &str) -> Result<Option<StackRevision>> {
        Ok(sqlx::query_as!(
            StackRevision,
            r#"SELECT r.id AS "id: RevisionId", r.stack, r.revision AS "revision: u32", r.toml,
                      r.hash AS "hash: ContentHash", r.created_at AS "created_at: Timestamp"
               FROM stack_revisions r JOIN stacks s ON s.name = r.stack
               WHERE r.stack = ? AND s.removed_at IS NULL
               ORDER BY r.revision DESC LIMIT 1"#,
            stack,
        )
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Every revision of a stack, newest first.
    pub async fn revisions(&self, stack: &str) -> Result<Vec<StackRevision>> {
        Ok(sqlx::query_as!(
            StackRevision,
            r#"SELECT id AS "id: RevisionId", stack, revision AS "revision: u32", toml,
                      hash AS "hash: ContentHash", created_at AS "created_at: Timestamp"
               FROM stack_revisions WHERE stack = ? ORDER BY revision DESC"#,
            stack,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// The current per-service pointers of a stack, by service name.
    pub async fn services(&self, stack: &str) -> Result<Vec<ServicePointer>> {
        let mut conn = self.pool.acquire().await?;
        let rows = sqlx::query!(
            r#"SELECT name, generation AS "generation: u32", digest, config_hash
               FROM services WHERE stack = ? ORDER BY name"#,
            stack,
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut ports: BTreeMap<String, PortMap> = BTreeMap::new();
        for p in sqlx::query!(
            r#"SELECT service, container_port AS "container_port: u16",
                      host_port AS "host_port: u16"
               FROM service_ports WHERE stack = ?"#,
            stack,
        )
        .fetch_all(&mut *conn)
        .await?
        {
            ports
                .entry(p.service)
                .or_default()
                .insert(p.container_port, p.host_port);
        }
        Ok(rows
            .into_iter()
            .map(|row| ServicePointer {
                ports: ports.remove(&row.name).unwrap_or_default(),
                service: row.name,
                generation: row.generation,
                digest: row.digest,
                config_hash: row.config_hash,
            })
            .collect())
    }

    // ---------------------------------------------------------------------------------------
    // Deployments
    // ---------------------------------------------------------------------------------------

    /// Records a new deployment in state `Received`, with its first event.
    pub async fn create_deployment(&self, new: &NewDeployment) -> Result<Deployment> {
        let now = Timestamp::now();
        let state = DeploymentState::Received;
        let mut tx = self.begin().await?;
        let exists = sqlx::query_scalar!(
            "SELECT 1 AS one FROM stacks WHERE name = ? AND removed_at IS NULL",
            new.stack
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !exists {
            return Err(StoreError::UnknownStack(new.stack.clone()));
        }
        let provenance = new.provenance.as_ref();
        let (repository, sha, git_ref, run_id, actor, tag) = (
            provenance.map(|p| &p.repository),
            provenance.map(|p| &p.sha),
            provenance.map(|p| &p.git_ref),
            provenance.map(|p| &p.run_id),
            provenance.map(|p| &p.actor),
            provenance.and_then(|p| p.tag.as_ref()),
        );
        let id = sqlx::query_scalar!(
            r#"INSERT INTO deployments
                   (stack, revision_id, state, source, created_at, updated_at, source_repository,
                    source_sha, source_ref, source_run_id, source_actor, source_tag)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!: DeploymentId""#,
            new.stack,
            new.revision,
            state,
            new.source,
            now,
            now,
            repository,
            sha,
            git_ref,
            run_id,
            actor,
            tag,
        )
        .fetch_one(&mut *tx)
        .await?;
        for (service, digest) in &new.target {
            sqlx::query!(
                "INSERT INTO deployment_targets (deployment_id, service, digest) VALUES (?, ?, ?)",
                id,
                service,
                digest,
            )
            .execute(&mut *tx)
            .await?;
        }
        let event = Self::log_event(&mut tx, id, None, state, None, now).await?;
        let deployment = Self::load_deployment(&mut tx, id)
            .await?
            .ok_or(StoreError::UnknownDeployment(id))?;
        tx.commit().await?;
        event.log(id, &new.stack);
        Ok(deployment)
    }

    /// Replaces a deployment's target digest set. A queued deployment's target is re-planned
    /// when it starts (ADR-0040): services it does not name take whatever runs by then, and the
    /// record must say what was actually deployed, because a rollback redeploys it (ADR-0041).
    pub async fn set_target(&self, id: DeploymentId, target: &DigestSet) -> Result<()> {
        let mut tx = self.begin().await?;
        let done = sqlx::query!(
            "UPDATE deployments SET updated_at = ? WHERE id = ?",
            Timestamp::now(),
            id,
        )
        .execute(&mut *tx)
        .await?;
        if done.rows_affected() == 0 {
            return Err(StoreError::UnknownDeployment(id));
        }
        sqlx::query!("DELETE FROM deployment_targets WHERE deployment_id = ?", id)
            .execute(&mut *tx)
            .await?;
        for (service, digest) in target {
            sqlx::query!(
                "INSERT INTO deployment_targets (deployment_id, service, digest) VALUES (?, ?, ?)",
                id,
                service,
                digest,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Takes the stack's next generation for a deployment (its `Validating` step, §5.1). A
    /// removed stack hands out none, so a deployment that slipped past its removal is rejected
    /// before it creates anything.
    pub async fn assign_generation(&self, id: DeploymentId) -> Result<u32> {
        let mut tx = self.begin().await?;
        let stack = sqlx::query_scalar!("SELECT stack FROM deployments WHERE id = ?", id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::UnknownDeployment(id))?;
        let generation = sqlx::query_scalar!(
            r#"UPDATE stacks SET last_generation = last_generation + 1
               WHERE name = ? AND removed_at IS NULL
               RETURNING last_generation AS "generation!: u32""#,
            stack,
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::UnknownStack(stack.clone()))?;
        sqlx::query!(
            "UPDATE deployments SET generation = ?, updated_at = ? WHERE id = ?",
            generation,
            Timestamp::now(),
            id,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(generation)
    }

    /// Moves a deployment to `to` and logs the transition. A terminal state sets `finished_at`;
    /// for `Failed` and `Rejected` the message is also kept as the deployment's error. Whether
    /// the transition is allowed is the orchestrator's business, not the store's.
    pub async fn transition(
        &self,
        id: DeploymentId,
        to: DeploymentState,
        message: Option<&str>,
    ) -> Result<()> {
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        let (stack, logged) = Self::transition_in(&mut tx, id, to, message, now).await?;
        tx.commit().await?;
        logged.log(id, &stack);
        Ok(())
    }

    /// [`transition`](Self::transition), recording `event` in the event log (ADR-0055) in the
    /// same transaction: the event is stored if and only if the transition is.
    pub async fn transition_recorded(
        &self,
        id: DeploymentId,
        to: DeploymentState,
        message: Option<&str>,
        event: &NewEvent,
    ) -> Result<Event> {
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        let (stack, logged) = Self::transition_in(&mut tx, id, to, message, now).await?;
        let recorded = Self::insert_event(&mut tx, event, now).await?;
        tx.commit().await?;
        logged.log(id, &stack);
        Ok(recorded)
    }

    /// The transition on `conn` (inside the caller's transaction): the deployment's stack
    /// and the logged transition, to print once the transaction commits.
    async fn transition_in(
        conn: &mut SqliteConnection,
        id: DeploymentId,
        to: DeploymentState,
        message: Option<&str>,
        now: Timestamp,
    ) -> Result<(String, DeploymentEvent)> {
        let finished_at = to.is_terminal().then_some(now);
        let error = matches!(to, DeploymentState::Failed | DeploymentState::Rejected)
            .then_some(message)
            .flatten();
        let from = sqlx::query_scalar!(
            r#"SELECT state AS "state: DeploymentState" FROM deployments WHERE id = ?"#,
            id,
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(StoreError::UnknownDeployment(id))?;
        sqlx::query!(
            "UPDATE deployments
             SET state = ?, updated_at = ?, finished_at = COALESCE(?, finished_at),
                 error = COALESCE(?, error)
             WHERE id = ?",
            to,
            now,
            finished_at,
            error,
            id,
        )
        .execute(&mut *conn)
        .await?;
        let stack = sqlx::query_scalar!("SELECT stack FROM deployments WHERE id = ?", id)
            .fetch_one(&mut *conn)
            .await?;
        let logged = Self::log_event(conn, id, Some(from), to, message, now).await?;
        Ok((stack, logged))
    }

    /// Logs a note on a deployment without changing its state: an event from `state` to
    /// `state` carrying `message` (health probe results, for example). Unlike
    /// [`transition`](Self::transition), the deployment row is left untouched.
    pub async fn note(
        &self,
        id: DeploymentId,
        state: DeploymentState,
        message: &str,
    ) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let stack = sqlx::query_scalar!("SELECT stack FROM deployments WHERE id = ?", id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(StoreError::UnknownDeployment(id))?;
        let event = Self::log_event(
            &mut conn,
            id,
            Some(state),
            state,
            Some(message),
            Timestamp::now(),
        )
        .await?;
        event.log(id, &stack);
        Ok(())
    }

    /// The deploy commit point (docs/architecture.md §5.1, §5.4): in one transaction, writes the
    /// current pointer of every service the deployment changed, drops the pointers of
    /// `removed` services, and marks the deployment committed. Either all of it is stored or
    /// none of it.
    pub async fn commit(
        &self,
        id: DeploymentId,
        pointers: &[ServicePointer],
        removed: &[String],
    ) -> Result<()> {
        let mut tx = self.begin().await?;
        Self::commit_in(&mut tx, id, pointers, removed, Timestamp::now()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// [`commit`](Self::commit), recording `event` (the deployment's success, ADR-0055) in the
    /// same transaction, so a committed deployment never lacks it and a failed commit never
    /// claims it.
    pub async fn commit_recorded(
        &self,
        id: DeploymentId,
        pointers: &[ServicePointer],
        removed: &[String],
        event: &NewEvent,
    ) -> Result<Event> {
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        Self::commit_in(&mut tx, id, pointers, removed, now).await?;
        let recorded = Self::insert_event(&mut tx, event, now).await?;
        tx.commit().await?;
        Ok(recorded)
    }

    /// The commit on `conn`, inside the caller's transaction.
    async fn commit_in(
        conn: &mut SqliteConnection,
        id: DeploymentId,
        pointers: &[ServicePointer],
        removed: &[String],
        now: Timestamp,
    ) -> Result<()> {
        let stack = sqlx::query_scalar!("SELECT stack FROM deployments WHERE id = ?", id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or(StoreError::UnknownDeployment(id))?;
        for p in pointers {
            sqlx::query!(
                "INSERT INTO services
                     (stack, name, generation, digest, config_hash, deployment_id, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT (stack, name) DO UPDATE SET
                     generation = excluded.generation, digest = excluded.digest,
                     config_hash = excluded.config_hash,
                     deployment_id = excluded.deployment_id, updated_at = excluded.updated_at",
                stack,
                p.service,
                p.generation,
                p.digest,
                p.config_hash,
                id,
                now,
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query!(
                "DELETE FROM service_ports WHERE stack = ? AND service = ?",
                stack,
                p.service,
            )
            .execute(&mut *conn)
            .await?;
            for (container_port, host_port) in &p.ports {
                sqlx::query!(
                    "INSERT INTO service_ports (stack, service, container_port, host_port)
                     VALUES (?, ?, ?, ?)",
                    stack,
                    p.service,
                    container_port,
                    host_port,
                )
                .execute(&mut *conn)
                .await?;
            }
        }
        for service in removed {
            sqlx::query!(
                "DELETE FROM services WHERE stack = ? AND name = ?",
                stack,
                service,
            )
            .execute(&mut *conn)
            .await?;
        }
        sqlx::query!(
            "UPDATE deployments SET committed_at = ?, updated_at = ? WHERE id = ?",
            now,
            now,
            id,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// One deployment, if it exists.
    pub async fn deployment(&self, id: DeploymentId) -> Result<Option<Deployment>> {
        let mut conn = self.pool.acquire().await?;
        Self::load_deployment(&mut conn, id).await
    }

    /// The newest `limit` deployments of a stack, newest first.
    pub async fn deployments(&self, stack: &str, limit: u32) -> Result<Vec<Deployment>> {
        let mut conn = self.pool.acquire().await?;
        let ids = sqlx::query_scalar!(
            r#"SELECT id AS "id: DeploymentId" FROM deployments
               WHERE stack = ? ORDER BY id DESC LIMIT ?"#,
            stack,
            limit,
        )
        .fetch_all(&mut *conn)
        .await?;
        Self::load_deployments(&mut conn, ids).await
    }

    /// The newest deployment of a stack that went live (`live` or `live_with_warnings`): the one
    /// a redeploy of the stack's last good state rolls back to (ADR-0041).
    pub async fn latest_live_deployment(&self, stack: &str) -> Result<Option<Deployment>> {
        let mut conn = self.pool.acquire().await?;
        let id = sqlx::query_scalar!(
            r#"SELECT id AS "id: DeploymentId" FROM deployments
               WHERE stack = ? AND state IN ('live', 'live_with_warnings')
               ORDER BY id DESC LIMIT 1"#,
            stack,
        )
        .fetch_optional(&mut *conn)
        .await?;
        match id {
            Some(id) => Self::load_deployment(&mut conn, id).await,
            None => Ok(None),
        }
    }

    /// How the deployments of every stack (not removed) that finished at or after `since` went:
    /// one entry per stack and terminal state, sorted by stack.
    pub async fn deployment_outcomes(&self, since: Timestamp) -> Result<Vec<DeploymentOutcome>> {
        let rows = sqlx::query!(
            r#"SELECT d.stack AS "stack!", d.state AS "state!: DeploymentState",
                      COUNT(*) AS "count!: i64",
                      AVG(d.finished_at - d.created_at) AS "mean_ms!: f64",
                      MAX(d.finished_at - d.created_at) AS "max_ms!: i64"
               FROM deployments d JOIN stacks s ON s.name = d.stack
               WHERE d.finished_at IS NOT NULL AND d.finished_at >= ? AND s.removed_at IS NULL
               GROUP BY d.stack, d.state
               ORDER BY d.stack, d.state"#,
            since,
        )
        .fetch_all(&self.pool)
        .await?;
        let millis = |ms: f64| Duration::from_secs_f64(ms.max(0.0) / 1000.0);
        Ok(rows
            .into_iter()
            .map(|row| DeploymentOutcome {
                stack: row.stack,
                state: row.state,
                count: u32::try_from(row.count).unwrap_or(u32::MAX),
                mean: millis(row.mean_ms),
                max: millis(row.max_ms as f64),
            })
            .collect())
    }

    /// Deployments that have not reached a terminal state, oldest first: what crash recovery
    /// (§5.4) has to resolve at startup.
    pub async fn unfinished_deployments(&self) -> Result<Vec<Deployment>> {
        let mut conn = self.pool.acquire().await?;
        let ids = sqlx::query_scalar!(
            r#"SELECT id AS "id: DeploymentId" FROM deployments
               WHERE finished_at IS NULL ORDER BY id"#,
        )
        .fetch_all(&mut *conn)
        .await?;
        Self::load_deployments(&mut conn, ids).await
    }

    /// The transition log of a deployment, oldest first.
    pub async fn events(&self, id: DeploymentId) -> Result<Vec<DeploymentEvent>> {
        Ok(sqlx::query_as!(
            DeploymentEvent,
            r#"SELECT from_state AS "from: DeploymentState", to_state AS "to: DeploymentState",
                      message, at AS "at: Timestamp"
               FROM deployment_events WHERE deployment_id = ? ORDER BY id"#,
            id,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Appends an event to a deployment's transition log. The caller writes the returned event
    /// to hoster's log once the transaction has committed.
    async fn log_event(
        conn: &mut SqliteConnection,
        id: DeploymentId,
        from: Option<DeploymentState>,
        to: DeploymentState,
        message: Option<&str>,
        at: Timestamp,
    ) -> Result<DeploymentEvent> {
        sqlx::query!(
            "INSERT INTO deployment_events (deployment_id, from_state, to_state, message, at)
             VALUES (?, ?, ?, ?, ?)",
            id,
            from,
            to,
            message,
            at,
        )
        .execute(conn)
        .await?;
        Ok(DeploymentEvent {
            from,
            to,
            message: message.map(str::to_owned),
            at,
        })
    }

    async fn load_deployments(
        conn: &mut SqliteConnection,
        ids: Vec<DeploymentId>,
    ) -> Result<Vec<Deployment>> {
        let mut deployments = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(d) = Self::load_deployment(conn, id).await? {
                deployments.push(d);
            }
        }
        Ok(deployments)
    }

    async fn load_deployment(
        conn: &mut SqliteConnection,
        id: DeploymentId,
    ) -> Result<Option<Deployment>> {
        let Some(row) = sqlx::query!(
            r#"SELECT id AS "id: DeploymentId", stack, revision_id AS "revision: RevisionId",
                      generation AS "generation: u32", state AS "state: DeploymentState",
                      source AS "source: DeploymentSource", error,
                      created_at AS "created_at: Timestamp", updated_at AS "updated_at: Timestamp",
                      committed_at AS "committed_at: Timestamp",
                      finished_at AS "finished_at: Timestamp",
                      source_repository, source_sha, source_ref, source_run_id, source_actor,
                      source_tag
               FROM deployments WHERE id = ?"#,
            id,
        )
        .fetch_optional(&mut *conn)
        .await?
        else {
            return Ok(None);
        };
        let target = sqlx::query!(
            "SELECT service, digest FROM deployment_targets WHERE deployment_id = ?",
            id,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|t| (t.service, t.digest))
        .collect();
        let provenance = match (
            row.source_repository,
            row.source_sha,
            row.source_ref,
            row.source_run_id,
            row.source_actor,
        ) {
            (Some(repository), Some(sha), Some(git_ref), Some(run_id), Some(actor)) => {
                Some(Provenance {
                    repository,
                    sha,
                    git_ref,
                    run_id,
                    actor,
                    tag: row.source_tag,
                })
            }
            _ => None,
        };
        Ok(Some(Deployment {
            id: row.id,
            stack: row.stack,
            revision: row.revision,
            generation: row.generation,
            state: row.state,
            source: row.source,
            target,
            provenance,
            error: row.error,
            created_at: row.created_at,
            updated_at: row.updated_at,
            committed_at: row.committed_at,
            finished_at: row.finished_at,
        }))
    }

    // ---------------------------------------------------------------------------------------
    // Port allocations (ADR-0026)
    // ---------------------------------------------------------------------------------------

    /// Reserves `allocation.port`. Fails with [`StoreError::PortTaken`] if it is reserved
    /// already. Choosing the port is the allocator's job.
    pub async fn reserve_port(&self, allocation: &PortAllocation) -> Result<()> {
        let result = sqlx::query!(
            "INSERT INTO port_allocations
                 (port, stack, service, container_port, generation, allocated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            allocation.port,
            allocation.stack,
            allocation.service,
            allocation.container_port,
            allocation.generation,
            Timestamp::now(),
        )
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                Err(StoreError::PortTaken(allocation.port))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// kv key of the port allocator's cursor: the last port it handed out (ADR-0026).
    const PORT_CURSOR_KEY: &str = "port_allocator.last";

    /// Allocates a port in one write transaction, so concurrent claims never get the same port.
    ///
    /// `choose` is given the allocator cursor (the port last handed out, if any) and every
    /// reserved port, and picks the port to reserve; the store records the allocation and moves
    /// the cursor to it. Returns `None`, storing nothing, when `choose` finds no port. The
    /// policy is the allocator's; the store only makes it atomic.
    pub async fn claim_port(
        &self,
        request: &PortRequest,
        choose: impl FnOnce(Option<u16>, &BTreeSet<u16>) -> Option<u16>,
    ) -> Result<Option<PortAllocation>> {
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        let exists = sqlx::query_scalar!(
            "SELECT 1 AS one FROM stacks WHERE name = ? AND removed_at IS NULL",
            request.stack
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if !exists {
            return Err(StoreError::UnknownStack(request.stack.clone()));
        }
        let cursor =
            sqlx::query_scalar!("SELECT value FROM kv WHERE key = ?", Self::PORT_CURSOR_KEY)
                .fetch_optional(&mut *tx)
                .await?
                .and_then(|value| value.parse::<u16>().ok());
        let taken: BTreeSet<u16> =
            sqlx::query_scalar!(r#"SELECT port AS "port: u16" FROM port_allocations"#)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .collect();
        let Some(port) = choose(cursor, &taken) else {
            return Ok(None);
        };
        let allocation = request.at(port);
        let inserted = sqlx::query!(
            "INSERT INTO port_allocations
                 (port, stack, service, container_port, generation, allocated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            allocation.port,
            allocation.stack,
            allocation.service,
            allocation.container_port,
            allocation.generation,
            now,
        )
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return Err(StoreError::PortTaken(port));
            }
            Err(e) => return Err(e.into()),
        }
        let value = port.to_string();
        sqlx::query!(
            "INSERT INTO kv (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            Self::PORT_CURSOR_KEY,
            value,
            now,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(allocation))
    }

    /// Releases a port. Returns whether it was reserved.
    pub async fn release_port(&self, port: u16) -> Result<bool> {
        let done = sqlx::query!("DELETE FROM port_allocations WHERE port = ?", port)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Every reserved port, in port order.
    pub async fn port_allocations(&self) -> Result<Vec<PortAllocation>> {
        Ok(sqlx::query_as!(
            PortAllocation,
            r#"SELECT port AS "port: u16", stack, service, container_port AS "container_port: u16",
                      generation AS "generation: u32"
               FROM port_allocations ORDER BY port"#,
        )
        .fetch_all(&self.pool)
        .await?)
    }

    // ---------------------------------------------------------------------------------------
    // Key-value items
    // ---------------------------------------------------------------------------------------

    /// A small stored item.
    pub async fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT value FROM kv WHERE key = ?", key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Stores (or replaces) a small item.
    pub async fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query!(
            "INSERT INTO kv (key, value, updated_at) VALUES (?, ?, ?)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            key,
            value,
            Timestamp::now(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes a small item. Returns whether it existed.
    pub async fn kv_remove(&self, key: &str) -> Result<bool> {
        let done = sqlx::query!("DELETE FROM kv WHERE key = ?", key)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests;
