//! Fakes for the updater, shared by its tests and the System page tests: a release source, the
//! deployment activity, a service and a temporary host with an installed layout and a migrated
//! database.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use reqwest::Url;
use tempfile::TempDir;

use super::*;

pub(crate) const TARGET: &str = "test-target";

pub(crate) fn v(s: &str) -> Version {
    s.parse().unwrap()
}

/// A fake `hoster` binary: a script answering `version --json` like the real one.
pub(crate) fn script(version: &str, schema: i64) -> Vec<u8> {
    format!(
        "#!/bin/sh\necho '{{\"version\":\"{version}\",\"target\":\"{TARGET}\",\"schema\":{schema}}}'\n"
    )
    .into_bytes()
}

#[derive(Default)]
pub(crate) struct FakeReleases {
    pub(crate) releases: Vec<Release>,
    pub(crate) files: HashMap<Url, Vec<u8>>,
}

impl FakeReleases {
    /// Publishes `version` with the given files.
    pub(crate) fn publish(mut self, version: &str, files: &[(&str, Vec<u8>)]) -> Self {
        let version = v(version);
        let assets = files
            .iter()
            .map(|(name, data)| {
                let url: Url =
                    format!("https://github.com/LHelge/hoster/releases/download/{version}/{name}")
                        .parse()
                        .unwrap();
                self.files.insert(url.clone(), data.clone());
                Asset {
                    name: (*name).to_owned(),
                    url,
                    size: Some(data.len() as u64),
                }
            })
            .collect();
        self.releases.push(Release {
            version: version.clone(),
            notes: format!("notes for {version}"),
            assets,
            published: None,
            page: None,
        });
        self
    }

    /// Publishes `version` with its binary and a matching `SHA256SUMS`.
    pub(crate) fn publish_binary(self, version: &str, binary: Vec<u8>) -> Self {
        let name = Release::binary_name(TARGET);
        let sums = format!(
            "{}  {name}\n{}  hoster-other\n",
            Sha256Digest::from(binary.as_slice()),
            Sha256Digest::from(b"other".as_slice())
        );
        self.publish(
            version,
            &[(&name, binary), (Release::CHECKSUMS, sums.into_bytes())],
        )
    }
}

#[async_trait]
impl ReleaseSource for FakeReleases {
    async fn latest(&self) -> Result<Release, SourceError> {
        self.releases
            .iter()
            .filter(|r| !r.version.is_prerelease())
            .max_by(|a, b| a.version.cmp(&b.version))
            .cloned()
            .ok_or(SourceError::NoRelease)
    }

    async fn release(&self, version: &Version) -> Result<Release, SourceError> {
        self.releases
            .iter()
            .find(|r| r.version == *version)
            .cloned()
            .ok_or_else(|| SourceError::UnknownRelease(version.clone()))
    }

    async fn download(&self, asset: &Asset, limit: u64, dest: &Path) -> Result<(), SourceError> {
        let data = &self.files[&asset.url];
        if data.len() as u64 > limit {
            return Err(SourceError::TooLarge {
                url: asset.url.clone(),
                limit,
            });
        }
        let io = |source| SourceError::Io {
            path: dest.to_owned(),
            source,
        };
        use std::io::Write;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .and_then(|mut f| f.write_all(data))
            .map_err(io)
    }
}

#[derive(Default)]
pub(crate) struct FakeDeployments(pub(crate) AtomicUsize);

#[async_trait]
impl DeploymentActivity for FakeDeployments {
    async fn in_progress(&self) -> Result<usize, StoreError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Default)]
pub(crate) struct FakeService {
    pub(crate) calls: Mutex<Vec<ServiceAction>>,
    pub(crate) fail: Option<ServiceAction>,
}

impl FakeService {
    pub(crate) fn calls(&self) -> Vec<ServiceAction> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ServiceControl for FakeService {
    async fn perform(&self, action: ServiceAction) -> Result<(), ServiceError> {
        self.calls.lock().unwrap().push(action);
        if self.fail == Some(action) {
            return Err(ServiceError {
                action,
                detail: "fake failure".into(),
            });
        }
        Ok(())
    }
}

pub(crate) struct Host {
    pub(crate) _dir: TempDir,
    pub(crate) installation: Installation,
    pub(crate) state_dir: PathBuf,
    pub(crate) deployments: Arc<FakeDeployments>,
    pub(crate) service: Arc<FakeService>,
}

impl Host {
    /// A host with `installed` versions (version, schema its binary reports), the last one
    /// current, and a migrated database.
    pub(crate) async fn new(installed: &[(&str, i64)]) -> Self {
        Self::with_service(installed, FakeService::default()).await
    }

    pub(crate) async fn with_service(installed: &[(&str, i64)], service: FakeService) -> Self {
        let dir = TempDir::new().unwrap();
        let installation = Installation::new(dir.path().join("lib"));
        let source = dir.path().join("source");
        for (version, schema) in installed {
            fs::write(&source, script(version, *schema)).unwrap();
            installation.install(&v(version), &source).unwrap();
        }
        let _ = fs::remove_file(&source);
        if let Some((last, _)) = installed.last() {
            installation.switch(&v(last)).unwrap();
        }
        let state_dir = dir.path().join("state");
        Store::open(&state_dir).await.unwrap().close().await;
        Self {
            _dir: dir,
            installation,
            state_dir,
            deployments: Arc::default(),
            service: Arc::new(service),
        }
    }

    pub(crate) fn updater(&self, source: FakeReleases, running: &str) -> Updater {
        Updater::new(
            self.installation.clone(),
            &self.state_dir,
            Arc::new(source),
            self.deployments.clone(),
            self.service.clone(),
        )
        .with_running(BuildInfo {
            version: running.parse().unwrap(),
            target: TARGET.into(),
            schema: Store::expected_schema_version().into(),
        })
    }

    pub(crate) fn current(&self) -> Option<Version> {
        self.installation.current().unwrap()
    }

    /// Files in the state directory besides the database.
    pub(crate) fn snapshots(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| !n.starts_with(Store::FILE_NAME))
            .collect();
        names.sort();
        names
    }

    /// Anything left in the layout root besides `versions` and `current`.
    pub(crate) fn leftovers(&self) -> Vec<String> {
        fs::read_dir(self.installation.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != "versions" && n != "current")
            .collect()
    }

    pub(crate) async fn store(&self) -> Store {
        Store::open_read_only(&self.state_dir).await.unwrap()
    }

    pub(crate) async fn schema(&self) -> SchemaVersion {
        let store = self.store().await;
        let schema = store.schema_version().await.unwrap();
        store.close().await;
        schema
    }

    /// Runs raw SQL against the (stopped) database.
    pub(crate) async fn sql(&self, sql: &'static str) {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(self.state_dir.join(Store::FILE_NAME));
        let mut conn = sqlx::ConnectOptions::connect(&options).await.unwrap();
        sqlx::raw_sql(sql).execute(&mut conn).await.unwrap();
        sqlx::Connection::close(conn).await.unwrap();
    }

    pub(crate) async fn count(&self, sql: &'static str) -> i64 {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(self.state_dir.join(Store::FILE_NAME))
            .read_only(true);
        let mut conn = sqlx::ConnectOptions::connect(&options).await.unwrap();
        let n = sqlx::query_scalar(sql).fetch_one(&mut conn).await.unwrap();
        sqlx::Connection::close(conn).await.unwrap();
        n
    }
}
