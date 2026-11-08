//! Self-update and rollback (ADR-0052), shared by `hoster update` / `hoster rollback` and the
//! admin UI's System page.
//!
//! [`Updater`] is the whole API:
//!
//! - [`check`](Updater::check) asks the [`ReleaseSource`] (GitHub) for the latest stable
//!   release and compares it with the running version.
//! - [`update`](Updater::update) refuses while a deployment is in progress, downloads
//!   `hoster-<target>` and `SHA256SUMS`, verifies the binary, installs it under `versions/`,
//!   snapshots the database into the state directory, switches `current` and prunes to the
//!   current and previous version. The caller then restarts hoster (graceful shutdown from the
//!   UI, `systemctl --user restart hoster` from the CLI).
//! - [`rollback`](Updater::rollback), run by the newer binary, reads the previous binary's
//!   schema (`hoster version --json`), stops the service, snapshots the database, undoes the
//!   migrations down to that schema, switches `current` back and starts the service. A failed
//!   undo restores the snapshot and leaves `current` as it was.
//!
//! Every failure before the switch leaves the layout as it was: downloads go to a temporary
//! directory that is always removed, and a version installed for a failed update is removed
//! again.

#![allow(
    dead_code,
    reason = "consumed by `hoster update` / `hoster rollback` and the System page (ADR-0052)"
)]

mod github;
mod release;
mod service;

#[cfg(test)]
mod github_tests;
#[cfg(test)]
mod tests;

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use thiserror::Error;

use crate::installation::{BuildInfo, Installation, InstallationError, Version};
use crate::store::{SchemaVersion, Store, StoreError};

#[allow(
    unused_imports,
    reason = "part of the updater API for the CLI and the UI"
)]
pub use github::{GithubReleases, UrlPolicy};
#[allow(
    unused_imports,
    reason = "part of the updater API for the CLI and the UI"
)]
pub use release::{
    Asset, Checksums, ChecksumsError, Release, ReleaseSource, Sha256Digest, SourceError,
};
#[allow(
    unused_imports,
    reason = "part of the updater API for the CLI and the UI"
)]
pub use service::{ServiceAction, ServiceControl, ServiceError, SystemdUserService};

/// Errors from checking, updating and rolling back. Each message says what to do next.
#[derive(Debug, Error)]
pub enum UpdateError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(
        "{count} deployment(s) are in progress; wait until they finish (or cancel them) and \
         update again"
    )]
    Deploying { count: usize },
    #[error("cannot tell whether a deployment is in progress")]
    Deployments(#[source] StoreError),
    #[error(
        "{requested} is older than the running {running}; update only moves forward, use \
         `hoster rollback` to return to the previous version"
    )]
    Downgrade {
        requested: Version,
        running: Version,
    },
    #[error(
        "release {version} has no {name}: this architecture is not published for it; nothing \
         was installed"
    )]
    MissingAsset { version: Version, name: String },
    #[error(
        "{} of release {version} does not list {name}; nothing was installed",
        Release::CHECKSUMS
    )]
    MissingChecksum { version: Version, name: String },
    #[error(transparent)]
    Checksums(#[from] ChecksumsError),
    #[error(
        "{name} does not match {}: expected {expected}, got {actual}; the download is corrupt \
         or was tampered with, nothing was installed",
        Release::CHECKSUMS
    )]
    ChecksumMismatch {
        name: String,
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    #[error("cannot use the download directory {}", path.display())]
    Download {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Installation(#[from] InstallationError),
    #[error("cannot snapshot the state database; nothing was switched")]
    Snapshot(#[source] StoreError),
    #[error(
        "no version is installed under {}; install hoster with install.sh first",
        root.display()
    )]
    NotInstalled { root: PathBuf },
    #[error("{current} is the only installed version; there is nothing to roll back to")]
    NoPrevious { current: Version },
    #[error(
        "the state database is at schema {database}, newer than this binary's {binary}; run \
         `hoster rollback` with the newest installed binary"
    )]
    SchemaAhead {
        database: SchemaVersion,
        binary: SchemaVersion,
    },
    #[error(
        "cannot undo the database schema to {target}; the snapshot {} was restored and \
         `current` is unchanged",
        snapshot.display()
    )]
    Undo {
        target: SchemaVersion,
        snapshot: PathBuf,
        #[source]
        source: StoreError,
    },
    #[error(
        "undoing the schema to {target} left the database at {reached}; the snapshot {} was \
         restored and `current` is unchanged",
        snapshot.display()
    )]
    UndoIncomplete {
        target: SchemaVersion,
        reached: SchemaVersion,
        snapshot: PathBuf,
    },
    #[error(
        "{failure}; restoring the snapshot {} failed as well, so the hoster service is left \
         stopped: restore it by hand (see the manual rollback in docs/install.md)",
        snapshot.display()
    )]
    Restore {
        failure: Box<UpdateError>,
        snapshot: PathBuf,
        #[source]
        source: StoreError,
    },
    #[error(transparent)]
    Service(#[from] ServiceError),
}

type Result<T, E = UpdateError> = std::result::Result<T, E>;

/// Whether deployments are running, for refusing an update (ADR-0052).
#[async_trait]
pub trait DeploymentActivity: Send + Sync {
    /// How many deployments have not finished (queued ones included).
    async fn in_progress(&self) -> Result<usize, StoreError>;
}

#[async_trait]
impl DeploymentActivity for Store {
    async fn in_progress(&self) -> Result<usize, StoreError> {
        Ok(self.unfinished_deployments().await?.len())
    }
}

/// The running version against the latest stable release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    pub running: Version,
    pub latest: Version,
    /// The latest release's notes.
    pub notes: String,
}

impl UpdateCheck {
    /// Whether the latest release is newer than the running version.
    pub fn update_available(&self) -> bool {
        self.latest > self.running
    }
}

/// What [`Updater::update`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The running version already is the requested (or the latest) one.
    UpToDate { running: Version },
    /// `to` is installed and `current`; restart hoster to run it.
    Switched {
        from: Version,
        to: Version,
        /// The pre-update database snapshot; `None` when there was no database yet.
        snapshot: Option<PathBuf>,
        /// The versions deleted by pruning.
        pruned: Vec<Version>,
    },
}

/// What [`Updater::rollback`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackOutcome {
    pub from: Version,
    pub to: Version,
    /// The schema the database was undone to.
    pub schema: SchemaVersion,
    /// The pre-rollback database snapshot; `None` when there was no database.
    pub snapshot: Option<PathBuf>,
}

/// Self-update and rollback for one host (ADR-0052).
pub struct Updater {
    installation: Installation,
    state_dir: PathBuf,
    source: Arc<dyn ReleaseSource>,
    deployments: Arc<dyn DeploymentActivity>,
    service: Arc<dyn ServiceControl>,
    running: BuildInfo,
}

impl Updater {
    /// Upper bound for a downloaded binary (release binaries are a few tens of MiB).
    const BINARY_LIMIT: u64 = 256 * 1024 * 1024;
    /// Upper bound for `SHA256SUMS` (a handful of lines).
    const CHECKSUMS_LIMIT: u64 = 64 * 1024;

    /// An updater for the layout `installation` and the database under `state_dir`, running as
    /// the current binary.
    pub fn new(
        installation: Installation,
        state_dir: impl Into<PathBuf>,
        source: Arc<dyn ReleaseSource>,
        deployments: Arc<dyn DeploymentActivity>,
        service: Arc<dyn ServiceControl>,
    ) -> Self {
        Self {
            installation,
            state_dir: state_dir.into(),
            source,
            deployments,
            service,
            running: BuildInfo::running(),
        }
    }

    /// Pretends to be the binary `running` describes (tests).
    #[cfg(test)]
    fn with_running(mut self, running: BuildInfo) -> Self {
        self.running = running;
        self
    }

    fn running_version(&self) -> Version {
        self.running.version.clone().into()
    }

    /// Compares the running version with the latest stable release.
    pub async fn check(&self) -> Result<UpdateCheck> {
        let latest = self.source.latest().await?;
        Ok(UpdateCheck {
            running: self.running_version(),
            latest: latest.version,
            notes: latest.notes,
        })
    }

    /// Installs and switches to `version`, or to the latest stable release when `None`. See the
    /// module documentation for the sequence; the caller restarts hoster afterwards.
    pub async fn update(&self, version: Option<&Version>) -> Result<UpdateOutcome> {
        self.refuse_while_deploying().await?;
        let release = match version {
            Some(version) => self.source.release(version).await?,
            None => self.source.latest().await?,
        };
        let running = self.running_version();
        if release.version == running || (version.is_none() && release.version < running) {
            return Ok(UpdateOutcome::UpToDate { running });
        }
        if release.version < running {
            return Err(UpdateError::Downgrade {
                requested: release.version,
                running,
            });
        }

        let downloads = DownloadDir::create(self.installation.root())?;
        let binary = self.fetch_verified(&release, &downloads).await?;
        let to = release.version;
        let was_installed = self.installation.versions()?.contains(&to);
        // A failed install may leave the new (empty) version directory behind.
        let installed = self
            .installation
            .install(&to, &binary)
            .map_err(UpdateError::from);
        drop(downloads);

        let switched = match installed {
            Ok(_) => self.snapshot_and_switch(&running, &to).await,
            Err(err) => Err(err),
        };
        let snapshot = match switched {
            Ok(snapshot) => snapshot,
            Err(err) => {
                if !was_installed && let Err(e) = self.installation.uninstall(&to) {
                    tracing::warn!(version = %to, error = %e, "cannot remove the unused install");
                }
                return Err(err);
            }
        };
        let pruned = self.installation.prune().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "cannot prune old versions; they stay installed");
            Vec::new()
        });
        tracing::info!(from = %running, %to, "switched to the new version; restart to run it");
        Ok(UpdateOutcome::Switched {
            from: running,
            to,
            snapshot,
            pruned,
        })
    }

    async fn refuse_while_deploying(&self) -> Result<()> {
        match self.deployments.in_progress().await {
            Ok(0) => Ok(()),
            Ok(count) => Err(UpdateError::Deploying { count }),
            Err(e) => Err(UpdateError::Deployments(e)),
        }
    }

    /// Downloads `SHA256SUMS` and this target's binary into `dir` and checks the binary against
    /// it. Returns the verified binary's path.
    async fn fetch_verified(&self, release: &Release, dir: &DownloadDir) -> Result<PathBuf> {
        let name = Release::binary_name(&self.running.target);
        let missing = |name: &str| UpdateError::MissingAsset {
            version: release.version.clone(),
            name: name.to_owned(),
        };
        let binary_asset = release.asset(&name).ok_or_else(|| missing(&name))?;
        let sums_asset = release
            .asset(Release::CHECKSUMS)
            .ok_or_else(|| missing(Release::CHECKSUMS))?;

        let sums_path = dir.path().join(Release::CHECKSUMS);
        self.source
            .download(sums_asset, Self::CHECKSUMS_LIMIT, &sums_path)
            .await?;
        let sums = fs::read_to_string(&sums_path).map_err(|source| UpdateError::Download {
            path: sums_path.clone(),
            source,
        })?;
        let expected =
            sums.parse::<Checksums>()?
                .get(&name)
                .ok_or_else(|| UpdateError::MissingChecksum {
                    version: release.version.clone(),
                    name: name.clone(),
                })?;

        let binary = dir.path().join(&name);
        self.source
            .download(binary_asset, Self::BINARY_LIMIT, &binary)
            .await?;
        let actual = Sha256Digest::of_file(&binary).map_err(|source| UpdateError::Download {
            path: binary.clone(),
            source,
        })?;
        if actual != expected {
            return Err(UpdateError::ChecksumMismatch {
                name,
                expected,
                actual,
            });
        }
        Ok(binary)
    }

    /// Snapshots the database, checks once more that nothing is deploying and switches
    /// `current` to `to`. A failure removes the snapshot it took.
    async fn snapshot_and_switch(&self, from: &Version, to: &Version) -> Result<Option<PathBuf>> {
        let snapshot = self.snapshot(&format!("pre-update-{from}-to-{to}")).await?;
        let switched = async {
            // A deployment may have started while the binary was downloading.
            self.refuse_while_deploying().await?;
            self.installation.switch(to)?;
            Ok(())
        }
        .await;
        if let Err(err) = switched {
            if let Some(path) = &snapshot {
                let _ = fs::remove_file(path);
            }
            return Err(err);
        }
        Ok(snapshot)
    }

    /// Snapshots the database to `<state_dir>/hoster-<label>-<unix time>.db`; `None` when there
    /// is no database yet.
    async fn snapshot(&self, label: &str) -> Result<Option<PathBuf>> {
        if !self.state_dir.join(Store::FILE_NAME).is_file() {
            return Ok(None);
        }
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let path = self.state_dir.join(format!("hoster-{label}-{secs}.db"));
        let store = Store::open_read_only(&self.state_dir)
            .await
            .map_err(UpdateError::Snapshot)?;
        let result = store.snapshot(&path).await;
        store.close().await;
        result.map_err(UpdateError::Snapshot)?;
        tracing::info!(snapshot = %path.display(), "snapshotted the state database");
        Ok(Some(path))
    }

    /// The schema the database is at; `None` when there is no database.
    async fn database_schema(&self) -> Result<Option<SchemaVersion>> {
        if !self.state_dir.join(Store::FILE_NAME).is_file() {
            return Ok(None);
        }
        let store = Store::open_read_only(&self.state_dir)
            .await
            .map_err(UpdateError::Snapshot)?;
        let schema = store.schema_version().await;
        store.close().await;
        schema.map(Some).map_err(UpdateError::Snapshot)
    }

    /// Returns to the previous installed version, undoing the database schema to the one it
    /// expects. Run by the current (newer) binary; see the module documentation.
    pub async fn rollback(&self) -> Result<RollbackOutcome> {
        let current = self
            .installation
            .current()?
            .ok_or_else(|| UpdateError::NotInstalled {
                root: self.installation.root().to_owned(),
            })?;
        let previous = self
            .installation
            .previous()?
            .ok_or_else(|| UpdateError::NoPrevious {
                current: current.clone(),
            })?;
        let info = BuildInfo::probe(&self.installation.binary(&previous)).await?;
        let target = SchemaVersion::from(info.schema);
        let binary_schema = SchemaVersion::from(self.running.schema);
        let database = self.database_schema().await?;
        if let Some(database) = database
            && database > binary_schema
        {
            return Err(UpdateError::SchemaAhead {
                database,
                binary: binary_schema,
            });
        }
        // A database already at or below the target has nothing to undo.
        let target = database.map_or(target, |database| database.min(target));

        self.service.perform(ServiceAction::Stop).await?;
        let rolled_back = self.undo_and_switch(&current, &previous, target).await;
        if let Err(err) = rolled_back {
            // The database may be half undone: starting hoster on it would migrate it forward
            // again and write to it before the owner restores the snapshot by hand.
            if matches!(err, UpdateError::Restore { .. }) {
                return Err(err);
            }
            // `current` is unchanged and the database is as it was: bring the service back.
            if let Err(e) = self.service.perform(ServiceAction::Start).await {
                tracing::error!(error = %e, "cannot restart hoster after a failed rollback");
            }
            return Err(err);
        }
        let snapshot = rolled_back?;
        self.service.perform(ServiceAction::Start).await?;
        tracing::info!(from = %current, to = %previous, schema = %target, "rolled back");
        Ok(RollbackOutcome {
            from: current,
            to: previous,
            schema: target,
            snapshot,
        })
    }

    /// With the service stopped: snapshots the database, undoes its schema to `target` and
    /// switches `current` to `previous`. On failure the snapshot is restored.
    async fn undo_and_switch(
        &self,
        current: &Version,
        previous: &Version,
        target: SchemaVersion,
    ) -> Result<Option<PathBuf>> {
        let Some(snapshot) = self
            .snapshot(&format!("pre-rollback-{current}-to-{previous}"))
            .await?
        else {
            self.installation.switch(previous)?;
            return Ok(None);
        };
        let undone = match Store::undo_migrations(&self.state_dir, target).await {
            Ok(reached) if reached == target => Ok(()),
            Ok(reached) => Err(UpdateError::UndoIncomplete {
                target,
                reached,
                snapshot: snapshot.clone(),
            }),
            Err(source) => Err(UpdateError::Undo {
                target,
                snapshot: snapshot.clone(),
                source,
            }),
        }
        .and_then(|()| Ok(self.installation.switch(previous)?));
        match undone {
            Ok(()) => Ok(Some(snapshot)),
            Err(failure) => match Store::restore(&self.state_dir, &snapshot) {
                Ok(()) => Err(failure),
                Err(source) => Err(UpdateError::Restore {
                    failure: Box::new(failure),
                    snapshot,
                    source,
                }),
            },
        }
    }
}

/// A private (0700) directory for downloads inside the layout root, removed with everything in
/// it when dropped. Installing copies the verified binary out of it.
struct DownloadDir(PathBuf);

impl DownloadDir {
    fn create(root: &Path) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = root.join(format!(
            ".download-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let err = |source| UpdateError::Download {
            path: path.clone(),
            source,
        };
        fs::create_dir_all(root).map_err(err)?;
        let _ = fs::remove_dir_all(&path);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(err)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for DownloadDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
