//! State store: SQLite via sqlx (ADR-0022).
//!
//! [`Store`] is the only place in hoster that talks SQL. It owns a WAL-mode `SqlitePool` on
//! `<state_dir>/hoster.db`, runs the embedded migrations (`migrations/` at the crate root) when it
//! opens, and exposes typed async methods. The queries are compile-time checked against the
//! offline metadata in `.sqlx/`; after changing one, run `cargo sqlx prepare` (see README).
//!
//! The orchestrator is the logical single writer of desired state (ADR-0050); the pool
//! serializes SQLite writes underneath. [`Store::commit`] is the deploy commit point
//! (docs/architecture.md §5.4): it writes every per-service pointer of a deployment in one
//! transaction.

#![allow(
    dead_code,
    reason = "the Store API is consumed by the orchestrator and web UI"
)]

mod secrets;
mod sessions;
mod types;

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
    reason = "part of the Store API for its future callers"
)]
pub use types::{
    ContentHash, Deployment, DeploymentEvent, DeploymentId, DeploymentSource, DeploymentState,
    DigestSet, NewDeployment, PortAllocation, PortMap, PortRequest, RevisionId, SealedSecret,
    SecretEntry, ServicePointer, SessionRecord, Stack, StackRevision, Timestamp,
};

/// The embedded migrations from `migrations/`.
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
        // PRAGMAs cannot be described by the query macros.
        let report: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
            .fetch_all(&self.pool)
            .await
            .map_err(|source| StoreError::Open {
                path: self.path.clone(),
                source,
            })?;
        match report.as_slice() {
            [ok] if ok == "ok" => Ok(()),
            _ => Err(StoreError::Corrupt {
                path: self.path.clone(),
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
        let now = Timestamp::now();
        let hash = ContentHash::of(toml);
        let mut tx = self.begin().await?;
        sqlx::query!(
            "INSERT INTO stacks (name, created_at, updated_at) VALUES (?1, ?2, ?2)
             ON CONFLICT (name) DO UPDATE SET updated_at = excluded.updated_at, removed_at = NULL",
            stack,
            now,
        )
        .execute(&mut *tx)
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
        .fetch_one(&mut *tx)
        .await?;
        for (service, digest) in images {
            sqlx::query!(
                "INSERT INTO revision_images (revision_id, service, digest) VALUES (?, ?, ?)",
                revision.id,
                service,
                digest,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
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
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        let done = sqlx::query!(
            "UPDATE stacks SET removed_at = ?1, updated_at = ?1
             WHERE name = ?2 AND removed_at IS NULL",
            now,
            name,
        )
        .execute(&mut *tx)
        .await?;
        if done.rows_affected() == 0 {
            return Ok(false);
        }
        sqlx::query!("DELETE FROM services WHERE stack = ?", name)
            .execute(&mut *tx)
            .await?;
        sqlx::query!("DELETE FROM port_allocations WHERE stack = ?", name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
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
        let id = sqlx::query_scalar!(
            r#"INSERT INTO deployments (stack, revision_id, state, source, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?)
               RETURNING id AS "id!: DeploymentId""#,
            new.stack,
            new.revision,
            state,
            new.source,
            now,
            now,
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
        let finished_at = to.is_terminal().then_some(now);
        let error = matches!(to, DeploymentState::Failed | DeploymentState::Rejected)
            .then_some(message)
            .flatten();
        let mut tx = self.begin().await?;
        let from = sqlx::query_scalar!(
            r#"SELECT state AS "state: DeploymentState" FROM deployments WHERE id = ?"#,
            id,
        )
        .fetch_optional(&mut *tx)
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
        .execute(&mut *tx)
        .await?;
        let stack = sqlx::query_scalar!("SELECT stack FROM deployments WHERE id = ?", id)
            .fetch_one(&mut *tx)
            .await?;
        let event = Self::log_event(&mut tx, id, Some(from), to, message, now).await?;
        tx.commit().await?;
        event.log(id, &stack);
        Ok(())
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
        let now = Timestamp::now();
        let mut tx = self.begin().await?;
        let stack = sqlx::query_scalar!("SELECT stack FROM deployments WHERE id = ?", id)
            .fetch_optional(&mut *tx)
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
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                "DELETE FROM service_ports WHERE stack = ? AND service = ?",
                stack,
                p.service,
            )
            .execute(&mut *tx)
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
                .execute(&mut *tx)
                .await?;
            }
        }
        for service in removed {
            sqlx::query!(
                "DELETE FROM services WHERE stack = ? AND name = ?",
                stack,
                service,
            )
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query!(
            "UPDATE deployments SET committed_at = ?, updated_at = ? WHERE id = ?",
            now,
            now,
            id,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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
                      finished_at AS "finished_at: Timestamp"
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
        Ok(Some(Deployment {
            id: row.id,
            stack: row.stack,
            revision: row.revision,
            generation: row.generation,
            state: row.state,
            source: row.source,
            target,
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
