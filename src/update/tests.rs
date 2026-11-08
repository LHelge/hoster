use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use reqwest::Url;
use tempfile::TempDir;

use super::*;
use crate::store::{DeploymentSource, NewDeployment};

const TARGET: &str = "test-target";

fn v(s: &str) -> Version {
    s.parse().unwrap()
}

/// A fake `hoster` binary: a script answering `version --json` like the real one.
fn script(version: &str, schema: i64) -> Vec<u8> {
    format!(
        "#!/bin/sh\necho '{{\"version\":\"{version}\",\"target\":\"{TARGET}\",\"schema\":{schema}}}'\n"
    )
    .into_bytes()
}

#[derive(Default)]
struct FakeReleases {
    releases: Vec<Release>,
    files: HashMap<Url, Vec<u8>>,
}

impl FakeReleases {
    /// Publishes `version` with the given files.
    fn publish(mut self, version: &str, files: &[(&str, Vec<u8>)]) -> Self {
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
        });
        self
    }

    /// Publishes `version` with its binary and a matching `SHA256SUMS`.
    fn publish_binary(self, version: &str, binary: Vec<u8>) -> Self {
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
struct FakeDeployments(AtomicUsize);

#[async_trait]
impl DeploymentActivity for FakeDeployments {
    async fn in_progress(&self) -> Result<usize, StoreError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Default)]
struct FakeService {
    calls: Mutex<Vec<ServiceAction>>,
    fail: Option<ServiceAction>,
}

impl FakeService {
    fn calls(&self) -> Vec<ServiceAction> {
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

struct Host {
    _dir: TempDir,
    installation: Installation,
    state_dir: PathBuf,
    deployments: Arc<FakeDeployments>,
    service: Arc<FakeService>,
}

impl Host {
    /// A host with `installed` versions (version, schema its binary reports), the last one
    /// current, and a migrated database.
    async fn new(installed: &[(&str, i64)]) -> Self {
        Self::with_service(installed, FakeService::default()).await
    }

    async fn with_service(installed: &[(&str, i64)], service: FakeService) -> Self {
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

    fn updater(&self, source: FakeReleases, running: &str) -> Updater {
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

    fn current(&self) -> Option<Version> {
        self.installation.current().unwrap()
    }

    /// Files in the state directory besides the database.
    fn snapshots(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| !n.starts_with(Store::FILE_NAME))
            .collect();
        names.sort();
        names
    }

    /// Anything left in the layout root besides `versions` and `current`.
    fn leftovers(&self) -> Vec<String> {
        fs::read_dir(self.installation.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n != "versions" && n != "current")
            .collect()
    }

    async fn store(&self) -> Store {
        Store::open_read_only(&self.state_dir).await.unwrap()
    }

    async fn schema(&self) -> SchemaVersion {
        let store = self.store().await;
        let schema = store.schema_version().await.unwrap();
        store.close().await;
        schema
    }

    /// Runs raw SQL against the (stopped) database.
    async fn sql(&self, sql: &'static str) {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(self.state_dir.join(Store::FILE_NAME));
        let mut conn = sqlx::ConnectOptions::connect(&options).await.unwrap();
        sqlx::raw_sql(sql).execute(&mut conn).await.unwrap();
        sqlx::Connection::close(conn).await.unwrap();
    }

    async fn count(&self, sql: &'static str) -> i64 {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(self.state_dir.join(Store::FILE_NAME))
            .read_only(true);
        let mut conn = sqlx::ConnectOptions::connect(&options).await.unwrap();
        let n = sqlx::query_scalar(sql).fetch_one(&mut conn).await.unwrap();
        sqlx::Connection::close(conn).await.unwrap();
        n
    }
}

// -------------------------------------------------------------------------------------------
// Checksums
// -------------------------------------------------------------------------------------------

#[test]
fn checksums_parse_sha256sum_output() {
    let a = Sha256Digest::from(b"a".as_slice());
    let b = Sha256Digest::from(b"b".as_slice());
    let sums: Checksums = format!("{a}  hoster-x86_64\n{b} *dist/SHA-b\n\n")
        .parse()
        .unwrap();
    assert_eq!(sums.get("hoster-x86_64"), Some(a));
    assert_eq!(sums.get("SHA-b"), Some(b));
    assert_eq!(sums.get("other"), None);
    assert_eq!(
        a.to_string(),
        "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb"
    );
    assert_eq!(a.to_string().parse::<Sha256Digest>().unwrap(), a);
}

#[test]
fn malformed_checksums_are_rejected() {
    let a = Sha256Digest::from(b"a".as_slice());
    for bad in [
        "nothex  file".to_owned(),
        format!("{a}"),
        format!("{a}  x\n{a}  x"),
        format!("{}  x", "g".repeat(64)),
    ] {
        assert!(bad.parse::<Checksums>().is_err(), "{bad:?}");
    }
}

// -------------------------------------------------------------------------------------------
// check and update
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn check_compares_the_running_version_with_the_latest_stable_release() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    let source = FakeReleases::default()
        .publish_binary("0.2.0", script("0.2.0", 7))
        .publish_binary("0.3.0-rc.1", script("0.3.0-rc.1", 7));
    let check = host.updater(source, "0.1.0").check().await.unwrap();
    assert_eq!(
        (check.running.clone(), check.latest.clone()),
        (v("0.1.0"), v("0.2.0"))
    );
    assert_eq!(check.notes, "notes for v0.2.0");
    assert!(check.update_available());

    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", 7));
    let check = host.updater(source, "0.2.0").check().await.unwrap();
    assert!(!check.update_available());
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", 7));
    assert!(
        !host
            .updater(source, "0.2.1")
            .check()
            .await
            .unwrap()
            .update_available()
    );
}

#[tokio::test]
async fn update_installs_verifies_snapshots_switches_and_prunes() {
    let host = Host::new(&[("0.0.9", 6), ("0.1.0", 7)]).await;
    let binary = script("0.2.0", 7);
    let source = FakeReleases::default().publish_binary("0.2.0", binary.clone());
    let outcome = host.updater(source, "0.1.0").update(None).await.unwrap();
    let UpdateOutcome::Switched {
        from,
        to,
        snapshot,
        pruned,
    } = outcome
    else {
        panic!("not switched: {outcome:?}");
    };
    assert_eq!((from, to.clone()), (v("0.1.0"), v("0.2.0")));
    assert_eq!(pruned, [v("0.0.9")]);
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert_eq!(
        host.installation.versions().unwrap(),
        [v("0.1.0"), v("0.2.0")]
    );
    assert_eq!(fs::read(host.installation.binary(&to)).unwrap(), binary);

    let snapshot = snapshot.expect("a database snapshot");
    assert_eq!(snapshot.parent(), Some(host.state_dir.as_path()));
    let name = snapshot.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("hoster-pre-update-v0.1.0-to-v0.2.0-"),
        "{name}"
    );
    assert!(snapshot.is_file());
    assert!(host.leftovers().is_empty(), "{:?}", host.leftovers());
    assert!(
        host.service.calls().is_empty(),
        "update leaves the restart to the caller"
    );
}

#[tokio::test]
async fn update_to_a_named_pre_release() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    let source = FakeReleases::default()
        .publish_binary("0.2.0", script("0.2.0", 7))
        .publish_binary("0.3.0-rc.1", script("0.3.0-rc.1", 7));
    let outcome = host
        .updater(source, "0.1.0")
        .update(Some(&v("v0.3.0-rc.1")))
        .await
        .unwrap();
    assert!(matches!(outcome, UpdateOutcome::Switched { ref to, .. } if *to == v("0.3.0-rc.1")));
    assert_eq!(host.current(), Some(v("0.3.0-rc.1")));
}

#[tokio::test]
async fn nothing_to_do_when_up_to_date_and_no_downgrades() {
    let host = Host::new(&[("0.2.0", 7)]).await;
    let source = || FakeReleases::default().publish_binary("0.1.0", script("0.1.0", 6));
    assert_eq!(
        host.updater(source(), "0.2.0").update(None).await.unwrap(),
        UpdateOutcome::UpToDate {
            running: v("0.2.0")
        }
    );
    let err = host
        .updater(source(), "0.2.0")
        .update(Some(&v("0.1.0")))
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Downgrade { .. }), "{err}");
    assert!(err.to_string().contains("hoster rollback"), "{err}");
    let err = host
        .updater(source(), "0.2.0")
        .update(Some(&v("0.9.0")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, UpdateError::Source(SourceError::UnknownRelease(_))),
        "{err}"
    );
    assert_eq!(host.installation.versions().unwrap(), [v("0.2.0")]);
}

/// Asserts that a failed update left everything as it was.
fn assert_untouched(host: &Host) {
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert_eq!(host.installation.versions().unwrap(), [v("0.1.0")]);
    assert!(host.leftovers().is_empty(), "{:?}", host.leftovers());
    assert!(host.snapshots().is_empty(), "{:?}", host.snapshots());
}

#[tokio::test]
async fn a_checksum_mismatch_installs_nothing() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    let name = Release::binary_name(TARGET);
    let sums = format!(
        "{}  {name}\n",
        Sha256Digest::from(b"something else".as_slice())
    );
    let source = FakeReleases::default().publish(
        "0.2.0",
        &[
            (&name, script("0.2.0", 7)),
            (Release::CHECKSUMS, sums.into_bytes()),
        ],
    );
    let err = host
        .updater(source, "0.1.0")
        .update(None)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::ChecksumMismatch { .. }), "{err}");
    assert!(err.to_string().contains("nothing was installed"), "{err}");
    assert_untouched(&host);
}

#[tokio::test]
async fn a_missing_binary_checksum_or_checksum_entry_installs_nothing() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    let name = Release::binary_name(TARGET);
    let other_sums = format!("{}  hoster-other\n", Sha256Digest::from(b"x".as_slice()));

    let only_other_target = FakeReleases::default().publish(
        "0.2.0",
        &[
            ("hoster-other", b"x".to_vec()),
            (Release::CHECKSUMS, other_sums.clone().into_bytes()),
        ],
    );
    let err = host
        .updater(only_other_target, "0.1.0")
        .update(None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, UpdateError::MissingAsset { ref name, .. } if *name == Release::binary_name(TARGET)),
        "{err}"
    );
    assert_untouched(&host);

    let no_sums = FakeReleases::default().publish("0.2.0", &[(&name, script("0.2.0", 7))]);
    let err = host
        .updater(no_sums, "0.1.0")
        .update(None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, UpdateError::MissingAsset { ref name, .. } if name == Release::CHECKSUMS),
        "{err}"
    );
    assert_untouched(&host);

    let unlisted = FakeReleases::default().publish(
        "0.2.0",
        &[
            (&name, script("0.2.0", 7)),
            (Release::CHECKSUMS, other_sums.into_bytes()),
        ],
    );
    let err = host
        .updater(unlisted, "0.1.0")
        .update(None)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::MissingChecksum { .. }), "{err}");
    assert_untouched(&host);
}

#[tokio::test]
async fn update_is_refused_while_deploying() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    host.deployments.0.store(2, Ordering::SeqCst);
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", 7));
    let err = host
        .updater(source, "0.1.0")
        .update(None)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Deploying { count: 2 }), "{err}");
    assert_untouched(&host);
}

#[tokio::test]
async fn the_store_reports_unfinished_deployments() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    let store = Store::open(&host.state_dir).await.unwrap();
    assert_eq!(store.in_progress().await.unwrap(), 0);
    store.add_revision("notes", "x").await.unwrap();
    store
        .create_deployment(&NewDeployment {
            stack: "notes".into(),
            revision: None,
            source: DeploymentSource::Webhook,
            target: [("api".to_owned(), "sha256:aa".to_owned())]
                .into_iter()
                .collect(),
            provenance: None,
        })
        .await
        .unwrap();
    assert_eq!(store.in_progress().await.unwrap(), 1);
    store.close().await;
}

#[tokio::test]
async fn a_failed_snapshot_removes_the_new_version_and_keeps_current() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    // Not a database: the snapshot fails after the binary is installed.
    fs::write(
        host.state_dir.join(Store::FILE_NAME),
        b"not a database at all",
    )
    .unwrap();
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", 7));
    let err = host
        .updater(source, "0.1.0")
        .update(None)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Snapshot(_)), "{err}");
    assert_untouched(&host);
}

#[tokio::test]
async fn update_without_a_database_skips_the_snapshot() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    fs::remove_dir_all(&host.state_dir).unwrap();
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", 7));
    let outcome = host.updater(source, "0.1.0").update(None).await.unwrap();
    assert!(matches!(
        outcome,
        UpdateOutcome::Switched { snapshot: None, .. }
    ));
    assert_eq!(host.current(), Some(v("0.2.0")));
}

// -------------------------------------------------------------------------------------------
// rollback
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn rollback_undoes_two_migrations_and_switches_back() {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    host.sql(
        "INSERT INTO stacks (name, last_generation, created_at, updated_at) VALUES ('notes', 0, 1, 1);
         INSERT INTO images VALUES ('docker.io/library/postgres', 'sha256:cc', 1);",
    )
    .await;
    let outcome = host
        .updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap();
    assert_eq!((outcome.from, outcome.to), (v("0.2.0"), v("0.1.0")));
    assert_eq!(outcome.schema, SchemaVersion::from(5));
    assert_eq!(host.schema().await, SchemaVersion::from(5));
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert_eq!(
        host.service.calls(),
        [ServiceAction::Stop, ServiceAction::Start]
    );
    // The rows the older schema holds survive; the snapshot has the newer schema.
    assert_eq!(host.count("SELECT count(*) FROM stacks").await, 1);
    let snapshot = outcome.snapshot.expect("a snapshot");
    let name = snapshot.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("hoster-pre-rollback-v0.2.0-to-v0.1.0-"),
        "{name}"
    );
    let restored = TempDir::new().unwrap();
    fs::copy(&snapshot, restored.path().join(Store::FILE_NAME)).unwrap();
    let store = Store::open_read_only(restored.path()).await.unwrap();
    assert_eq!(
        store.schema_version().await.unwrap(),
        Store::expected_schema_version()
    );
    store.close().await;
}

#[tokio::test]
async fn a_failed_undo_restores_the_snapshot_and_keeps_current() {
    let host = Host::new(&[("0.1.0", 4), ("0.2.0", 7)]).await;
    // 0005's down script drops stacks.removed_at, which SQLite refuses for an indexed column:
    // 0007 and 0006 are undone, then the undo fails half-way.
    host.sql(
        "INSERT INTO stacks (name, last_generation, created_at, updated_at) VALUES ('notes', 0, 1, 1);
         CREATE INDEX stacks_by_removal ON stacks (removed_at);",
    )
    .await;
    let err = host
        .updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Undo { .. }), "{err}");
    assert!(err.to_string().contains("was restored"), "{err}");
    assert_eq!(host.schema().await, Store::expected_schema_version());
    assert_eq!(
        host.count("SELECT count(*) FROM sqlite_schema WHERE name = 'images'")
            .await,
        1
    );
    assert_eq!(host.count("SELECT count(*) FROM stacks").await, 1);
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert_eq!(
        host.service.calls(),
        [ServiceAction::Stop, ServiceAction::Start]
    );
    // The restored database opens and migrates as usual.
    Store::open(&host.state_dir).await.unwrap().close().await;
}

#[tokio::test]
async fn rollback_without_a_previous_version_fails_clearly() {
    let host = Host::new(&[("0.2.0", 7)]).await;
    let err = host
        .updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::NoPrevious { .. }), "{err}");
    assert!(err.to_string().contains("nothing to roll back to"), "{err}");
    assert!(host.service.calls().is_empty());

    let empty = Host::new(&[]).await;
    let err = empty
        .updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::NotInstalled { .. }), "{err}");
}

#[tokio::test]
async fn rollback_stops_before_the_service_when_the_previous_binary_cannot_answer() {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    let previous = host.installation.binary(&v("0.1.0"));
    fs::write(&previous, "#!/bin/sh\necho 'unknown command' >&2\nexit 2\n").unwrap();
    let err = host
        .updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            UpdateError::Installation(InstallationError::Probe { .. })
        ),
        "{err}"
    );
    assert!(err.to_string().contains("docs/install.md"), "{err}");
    assert!(host.service.calls().is_empty());
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert_eq!(host.schema().await, Store::expected_schema_version());
}

#[tokio::test]
async fn rollback_refuses_a_database_newer_than_this_binary() {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    let updater = host
        .updater(FakeReleases::default(), "0.2.0")
        .with_running(BuildInfo {
            version: "0.2.0".parse().unwrap(),
            target: TARGET.into(),
            schema: 6,
        });
    let err = updater.rollback().await.unwrap_err();
    assert!(matches!(err, UpdateError::SchemaAhead { .. }), "{err}");
    assert!(host.service.calls().is_empty());
}

#[tokio::test]
async fn a_failed_stop_changes_nothing() {
    let service = FakeService {
        fail: Some(ServiceAction::Stop),
        ..FakeService::default()
    };
    let host = Host::with_service(&[("0.1.0", 5), ("0.2.0", 7)], service).await;
    let err = host
        .updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Service(_)), "{err}");
    assert!(
        err.to_string().contains("systemctl --user status hoster"),
        "{err}"
    );
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert_eq!(host.schema().await, Store::expected_schema_version());
    assert!(host.snapshots().is_empty());
}

#[tokio::test]
async fn a_failed_restore_leaves_the_service_stopped() {
    let host = Host::new(&[("0.1.0", 4), ("0.2.0", 7)]).await;
    // The undo fails at 0005 (see above), and the restore cannot write its temporary file.
    host.sql(
        "INSERT INTO stacks (name, last_generation, created_at, updated_at) VALUES ('notes', 0, 1, 1);
         CREATE INDEX stacks_by_removal ON stacks (removed_at);",
    )
    .await;
    let blocker = host
        .state_dir
        .join(format!(".{}.tmp-{}", Store::FILE_NAME, std::process::id()));
    fs::create_dir(&blocker).unwrap();
    let err = host
        .updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Restore { .. }), "{err}");
    assert!(err.to_string().contains("left stopped"), "{err}");
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert_eq!(
        host.service.calls(),
        [ServiceAction::Stop],
        "hoster must not start on a half-undone database"
    );
}

#[tokio::test]
async fn a_failed_install_leaves_no_version_directory() {
    let host = Host::new(&[("0.1.0", 7)]).await;
    // A directory where the binary goes: renaming the new binary over it fails.
    let stray = host.installation.binary(&v("0.2.0"));
    fs::create_dir_all(&stray).unwrap();
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", 7));
    let err = host
        .updater(source, "0.1.0")
        .update(None)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Installation(_)), "{err}");
    assert!(!stray.parent().unwrap().exists());
    assert_untouched(&host);
}
