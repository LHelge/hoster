use super::fake::*;
use super::*;
use crate::store::{DeploymentSource, EventKind, EventQuery, LogStore, NewDeployment};
use tempfile::TempDir;

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
    // The CLI's view opens the database read-only for each question.
    let state = StateDeployments::new(&host.state_dir);
    assert_eq!(state.in_progress().await.unwrap(), 1);
    let empty = TempDir::new().unwrap();
    assert_eq!(
        StateDeployments::new(empty.path())
            .in_progress()
            .await
            .unwrap(),
        0
    );
    assert!(!empty.path().join(Store::FILE_NAME).exists());
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
    assert!(
        err.to_string()
            .contains("https://docs.hoster.lhelge.se/wiki/install/"),
        "{err}"
    );
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
            log_schema: None,
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

// -------------------------------------------------------------------------------------------
// rollback of logs.db
// -------------------------------------------------------------------------------------------

/// A host rolling back from 0.2.0 (this binary) to 0.1.0 at schema 5 and log schema
/// `log_schema` (`None`: from before the log store), with a `logs.db` holding one record.
async fn host_with_logs(log_schema: Option<i64>) -> Host {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    host.reinstall("0.1.0", 5, log_schema);
    host.open_logs().await;
    host.log_sql(
        "INSERT INTO log_records (time, stack, service, stream, message) \
         VALUES (1, 'notes', 'api', 'stdout', 'hello')",
    )
    .await;
    host
}

#[tokio::test]
async fn rollback_undoes_the_log_store_to_the_previous_log_schema() {
    let host = host_with_logs(Some(1)).await;
    let updater = host.updater(FakeReleases::default(), "0.2.0");
    let plan = updater.rollback_plan().await.unwrap();
    assert_eq!(plan.logs, Some(LogStore::expected_schema_version()));
    assert_eq!(plan.logs_target, Some(SchemaVersion::from(1)));
    assert_eq!(
        plan.log_migrations_undone(),
        LogStore::migrations_between(SchemaVersion::from(1), LogStore::expected_schema_version())
    );
    assert!(plan.log_migrations_undone() >= 1);

    let outcome = updater.rollback().await.unwrap();
    assert_eq!(
        outcome.logs,
        LogsRollback::Undone {
            schema: SchemaVersion::from(1)
        }
    );
    assert_eq!(host.schema().await, SchemaVersion::from(5));
    assert_eq!(host.log_schema().await, Some(SchemaVersion::from(1)));
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert_eq!(
        host.service.calls(),
        [ServiceAction::Stop, ServiceAction::Start]
    );
    // What the older log schema holds survives, and the older binary's store opens.
    let logs = LogStore::open(&host.state_dir).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM log_records")
        .fetch_one(logs.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    logs.close().await;
}

#[tokio::test]
async fn a_previous_binary_without_a_log_store_leaves_logs_db_alone() {
    let host = host_with_logs(None).await;
    let updater = host.updater(FakeReleases::default(), "0.2.0");
    let plan = updater.rollback_plan().await.unwrap();
    assert_eq!(plan.logs_target, None);
    assert_eq!(plan.log_migrations_undone(), 0);

    let outcome = updater.rollback().await.unwrap();
    assert_eq!(outcome.logs, LogsRollback::Kept);
    assert_eq!(host.schema().await, SchemaVersion::from(5));
    assert_eq!(
        host.log_schema().await,
        Some(LogStore::expected_schema_version())
    );
}

#[tokio::test]
async fn rollback_without_a_log_store_or_with_nothing_to_undo_keeps_it() {
    let host = Host::new(&[("0.1.0", 5), ("0.2.0", 7)]).await;
    host.reinstall("0.1.0", 5, Some(1));
    let outcome = host
        .updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap();
    assert_eq!(outcome.logs, LogsRollback::Kept);
    assert!(
        !host.state_dir.join(LogStore::FILE_NAME).exists(),
        "never created"
    );

    let latest = i64::from(LogStore::expected_schema_version());
    let host = host_with_logs(Some(latest)).await;
    let updater = host.updater(FakeReleases::default(), "0.2.0");
    assert_eq!(
        updater
            .rollback_plan()
            .await
            .unwrap()
            .log_migrations_undone(),
        0
    );
    assert_eq!(updater.rollback().await.unwrap().logs, LogsRollback::Kept);
    assert_eq!(
        host.log_schema().await,
        Some(LogStore::expected_schema_version())
    );
}

#[tokio::test]
async fn a_failed_log_undo_moves_logs_db_aside_and_still_rolls_back() {
    let host = host_with_logs(Some(1)).await;
    // 0002's down script drops log_cursors.nth, which SQLite refuses for an indexed column.
    host.log_sql("CREATE INDEX cursors_by_nth ON log_cursors (nth)")
        .await;
    let outcome = host
        .updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap();
    let aside = host.state_dir.join("logs.db.rollback-v0.2.0");
    match &outcome.logs {
        LogsRollback::MovedAside { path, reason } => {
            assert_eq!(path, &aside);
            assert!(reason.contains("logs.db"), "{reason}");
        }
        other => panic!("expected the log store moved aside, got {other:?}"),
    }
    assert!(aside.is_file());
    assert!(!host.state_dir.join(LogStore::FILE_NAME).exists());
    // hoster.db is undone, not restored, and the previous version is started.
    assert_eq!(host.schema().await, SchemaVersion::from(5));
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert_eq!(
        host.service.calls(),
        [ServiceAction::Stop, ServiceAction::Start]
    );
    // The previous binary starts on a new, empty log store.
    LogStore::open(&host.state_dir).await.unwrap().close().await;
}

#[tokio::test]
async fn a_failed_database_undo_leaves_logs_db_untouched() {
    let host = Host::new(&[("0.1.0", 4), ("0.2.0", 7)]).await;
    host.reinstall("0.1.0", 4, Some(1));
    host.open_logs().await;
    // The undo of hoster.db fails at 0005 (see above).
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
    assert_eq!(
        host.log_schema().await,
        Some(LogStore::expected_schema_version())
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

#[test]
fn systemctl_gets_a_runtime_dir_only_when_the_session_has_none() {
    use std::ffi::OsStr;
    let fallback = Some(PathBuf::from("/run/user/1001"));
    // `sudo -iu hoster` sets no XDG_RUNTIME_DIR; `systemctl --user` would not find the manager.
    assert_eq!(
        SystemdUserService::fallback_runtime_dir(None, 1001),
        fallback
    );
    assert_eq!(
        SystemdUserService::fallback_runtime_dir(Some(OsStr::new("")), 1001),
        fallback
    );
    assert_eq!(
        SystemdUserService::fallback_runtime_dir(Some(OsStr::new("/run/user/1001")), 1001),
        None
    );
}

// -------------------------------------------------------------------------------------------
// event log (ADR-0055)
// -------------------------------------------------------------------------------------------

impl Host {
    /// The hoster events in the database, as (kind, meta), oldest first.
    async fn hoster_events(&self) -> Vec<(EventKind, String)> {
        let store = self.store().await;
        let query = EventQuery {
            kinds: [EventKind::HosterUpdated, EventKind::HosterRolledBack].into(),
            ..EventQuery::default()
        };
        let mut events = store.event_page(&query).await.unwrap().events;
        store.close().await;
        events.reverse();
        events
            .into_iter()
            .map(|e| (e.kind, e.meta.unwrap_or_default()))
            .collect()
    }
}

/// The schema this binary migrates to, which has the event log.
fn events_schema() -> i64 {
    Store::expected_schema_version().into()
}

/// The schema just before the migration that creates the event log.
fn before_events_schema() -> i64 {
    i64::from(Store::migration_version("events")) - 1
}

#[tokio::test]
async fn an_update_records_the_versions_after_the_switch_but_not_in_the_snapshot() {
    let host = Host::new(&[("0.1.0", events_schema())]).await;
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", events_schema()));
    let outcome = host.updater(source, "0.1.0").update(None).await.unwrap();
    assert_eq!(
        host.hoster_events().await,
        [(EventKind::HosterUpdated, "v0.1.0 → v0.2.0".to_owned())]
    );
    // The snapshot is the state before the update: restoring it forgets the update.
    let UpdateOutcome::Switched {
        snapshot: Some(snapshot),
        ..
    } = outcome
    else {
        panic!("not switched with a snapshot: {outcome:?}");
    };
    let restored = TempDir::new().unwrap();
    fs::copy(&snapshot, restored.path().join(Store::FILE_NAME)).unwrap();
    let store = Store::open_read_only(restored.path()).await.unwrap();
    assert!(
        store
            .event_page(&EventQuery::default())
            .await
            .unwrap()
            .events
            .is_empty()
    );
    store.close().await;
}

#[tokio::test]
async fn a_refused_or_needless_update_records_nothing() {
    let host = Host::new(&[("0.2.0", events_schema())]).await;
    let source =
        || FakeReleases::default().publish_binary("0.2.0", script("0.2.0", events_schema()));
    host.updater(source(), "0.2.0").update(None).await.unwrap();
    host.deployments.0.store(1, Ordering::SeqCst);
    let source = FakeReleases::default().publish_binary("0.3.0", script("0.3.0", events_schema()));
    assert!(host.updater(source, "0.2.0").update(None).await.is_err());
    assert!(host.hoster_events().await.is_empty());
}

#[tokio::test]
async fn a_rollback_records_the_versions() {
    let host = Host::new(&[("0.1.0", events_schema()), ("0.2.0", events_schema())]).await;
    host.updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap();
    assert_eq!(
        host.hoster_events().await,
        [(EventKind::HosterRolledBack, "v0.2.0 → v0.1.0".to_owned())]
    );
}

#[tokio::test]
async fn a_rollback_to_a_schema_without_the_event_log_takes_the_event_with_it() {
    let host = Host::new(&[
        ("0.1.0", before_events_schema()),
        ("0.2.0", events_schema()),
    ])
    .await;
    // A witness outside the schema sees every event written while the table still exists.
    host.sql(
        "CREATE TABLE witness (kind TEXT NOT NULL, meta TEXT);
         CREATE TRIGGER events_witness AFTER INSERT ON events
         BEGIN INSERT INTO witness VALUES (new.kind, new.meta); END;",
    )
    .await;
    host.updater(FakeReleases::default(), "0.2.0")
        .rollback()
        .await
        .unwrap();
    assert_eq!(
        i64::from(host.schema().await),
        before_events_schema(),
        "rolled back past the event log"
    );
    assert_eq!(
        host.count("SELECT count(*) FROM sqlite_schema WHERE name = 'events'")
            .await,
        0
    );
    // The event was written before the down-migration dropped the table.
    assert_eq!(
        host.count("SELECT count(*) FROM witness WHERE meta = 'v0.2.0 → v0.1.0'")
            .await,
        1
    );
}

#[tokio::test]
async fn a_failed_rollback_leaves_no_rollback_event() {
    let host = Host::new(&[("0.1.0", 4), ("0.2.0", events_schema())]).await;
    // As in `a_failed_undo_restores_the_snapshot_and_keeps_current`: 0005's down script fails.
    host.sql(
        "INSERT INTO stacks (name, last_generation, created_at, updated_at) VALUES ('notes', 0, 1, 1);
         CREATE INDEX stacks_by_removal ON stacks (removed_at);",
    )
    .await;
    assert!(
        host.updater(FakeReleases::default(), "0.2.0")
            .rollback()
            .await
            .is_err()
    );
    assert!(host.hoster_events().await.is_empty());
}
