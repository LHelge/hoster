use std::os::unix::fs::PermissionsExt;

use tempfile::TempDir;

use super::*;

async fn open() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    (dir, store)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn target(pairs: &[(&str, &str)]) -> DigestSet {
    pairs
        .iter()
        .map(|(s, d)| (s.to_string(), d.to_string()))
        .collect()
}

/// A pointer whose service listens on container port 8080 if `port` is given.
fn pointer(service: &str, generation: u32, port: Option<u16>) -> ServicePointer {
    ServicePointer {
        service: service.into(),
        generation,
        digest: format!("sha256:{service}{generation}"),
        config_hash: format!("cfg-{service}"),
        ports: port.map(|p| (8080, p)).into_iter().collect(),
    }
}

async fn deployment(store: &Store, stack: &str) -> Deployment {
    store
        .create_deployment(&NewDeployment {
            stack: stack.into(),
            revision: None,
            source: DeploymentSource::Webhook,
            target: target(&[("api", "sha256:aa"), ("web", "sha256:bb")]),
            provenance: None,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn open_creates_private_state_dir_and_wal_database() {
    let (dir, store) = open().await;
    let state = dir.path().join("state");
    assert_eq!(mode(&state), 0o700);
    assert_eq!(store.path(), state.join(Store::FILE_NAME));
    assert_eq!(mode(store.path()), 0o600);

    // Write something so the WAL file exists, then check its mode and the pragmas.
    store.kv_set("k", "v").await.unwrap();
    assert_eq!(mode(&state.join("hoster.db-wal")), 0o600);
    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(journal, "wal");
    let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(fk, 1);
    store.close().await;
}

#[tokio::test]
async fn open_tightens_existing_permissions() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, Permissions::from_mode(0o755)).unwrap();
    fs::write(state.join(Store::FILE_NAME), b"").unwrap();
    fs::set_permissions(state.join(Store::FILE_NAME), Permissions::from_mode(0o644)).unwrap();
    // A WAL file left behind by an earlier run keeps the mode it was created with.
    let wal = state.join("hoster.db-wal");
    fs::write(&wal, b"").unwrap();
    fs::set_permissions(&wal, Permissions::from_mode(0o644)).unwrap();

    let store = Store::open(&state).await.unwrap();
    assert_eq!(mode(&wal), 0o600);
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(store.path()), 0o600);
}

#[tokio::test]
async fn migrations_are_idempotent_and_data_survives_reopen() {
    let (dir, store) = open().await;
    store.add_revision("blog", "a = 1").await.unwrap();
    store.close().await;

    let store = Store::open(&dir.path().join("state")).await.unwrap();
    assert_eq!(store.stacks().await.unwrap().len(), 1);
    let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(applied, MIGRATOR.iter().count() as i64);
}

#[tokio::test]
async fn corrupt_database_fails_to_open_with_clear_error() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::write(state.join(Store::FILE_NAME), vec![0x42; 8192]).unwrap();

    let err = Store::open(&state).await.unwrap_err();
    assert!(
        matches!(err, StoreError::Open { .. } | StoreError::Corrupt { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("hoster.db"), "{err}");
}

/// A file with a valid header but damaged pages opens as a database; startup must still fail.
#[tokio::test]
async fn damaged_pages_fail_to_open_with_clear_error() {
    let (dir, store) = open().await;
    for i in 0..20 {
        store
            .add_revision("blog", &"x".repeat(4096 + i))
            .await
            .unwrap();
    }
    store.close().await;
    let path = dir.path().join("state").join(Store::FILE_NAME);
    let mut bytes = fs::read(&path).unwrap();
    assert!(bytes.len() > 16 * 4096);
    // Keep the header page intact so the file still opens as a database.
    bytes[4096..].fill(0x42);
    fs::write(&path, bytes).unwrap();

    let err = Store::open(&dir.path().join("state")).await.unwrap_err();
    assert!(
        matches!(err, StoreError::Open { .. } | StoreError::Corrupt { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("hoster.db"), "{err}");
}

#[tokio::test]
async fn database_from_a_newer_hoster_is_refused() {
    let (dir, store) = open().await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (999999, 'from the future', 1, x'00', 0)",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    store.close().await;

    let err = Store::open(&dir.path().join("state")).await.unwrap_err();
    assert!(matches!(err, StoreError::Migrate { .. }), "{err:?}");
}

#[tokio::test]
async fn revisions_are_numbered_per_stack_and_hashed() {
    let (_dir, store) = open().await;
    let r1 = store.add_revision("blog", "one").await.unwrap();
    let r2 = store.add_revision("blog", "two").await.unwrap();
    let other = store.add_revision("shop", "one").await.unwrap();
    assert_eq!((r1.revision, r2.revision, other.revision), (1, 2, 1));
    assert_eq!(r1.hash, ContentHash::of("one"));
    assert_eq!(r1.hash, other.hash);
    assert_eq!(r2.toml, "two");

    assert_eq!(
        store.latest_revision("blog").await.unwrap(),
        Some(r2.clone())
    );
    assert_eq!(store.revision(r1.id).await.unwrap(), Some(r1.clone()));
    assert_eq!(store.revisions("blog").await.unwrap(), vec![r2, r1]);

    let names: Vec<_> = store
        .stacks()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, ["blog", "shop"]);
    assert_eq!(
        store.stack("blog").await.unwrap().unwrap().last_generation,
        0
    );
    assert_eq!(store.stack("nope").await.unwrap(), None);
}

#[tokio::test]
async fn deployment_lifecycle_is_logged() {
    let (_dir, store) = open().await;
    let rev = store.add_revision("blog", "x").await.unwrap();
    let d = store
        .create_deployment(&NewDeployment {
            stack: "blog".into(),
            revision: Some(rev.id),
            source: DeploymentSource::Apply,
            target: target(&[("api", "sha256:aa")]),
            provenance: None,
        })
        .await
        .unwrap();
    assert_eq!(d.state, DeploymentState::Received);
    assert_eq!(d.revision, Some(rev.id));
    assert_eq!(d.target, target(&[("api", "sha256:aa")]));
    assert_eq!(d.generation, None);

    store
        .transition(d.id, DeploymentState::Validating, None)
        .await
        .unwrap();
    assert_eq!(store.assign_generation(d.id).await.unwrap(), 1);
    store
        .transition(d.id, DeploymentState::Failed, Some("pull failed"))
        .await
        .unwrap();

    let d = store.deployment(d.id).await.unwrap().unwrap();
    assert_eq!(d.state, DeploymentState::Failed);
    assert_eq!(d.generation, Some(1));
    assert_eq!(d.error.as_deref(), Some("pull failed"));
    assert!(d.finished_at.is_some());
    assert_eq!(d.committed_at, None);

    let events = store.events(d.id).await.unwrap();
    let steps: Vec<_> = events.iter().map(|e| (e.from, e.to)).collect();
    assert_eq!(
        steps,
        [
            (None, DeploymentState::Received),
            (Some(DeploymentState::Received), DeploymentState::Validating),
            (Some(DeploymentState::Validating), DeploymentState::Failed),
        ]
    );
    assert_eq!(events[2].message.as_deref(), Some("pull failed"));

    // The next deployment gets the next generation.
    let d2 = deployment(&store, "blog").await;
    assert_eq!(store.assign_generation(d2.id).await.unwrap(), 2);
    assert_eq!(
        store.stack("blog").await.unwrap().unwrap().last_generation,
        2
    );
}

#[tokio::test]
async fn unknown_stack_and_deployment_are_errors() {
    let (_dir, store) = open().await;
    let err = store
        .create_deployment(&NewDeployment {
            stack: "ghost".into(),
            revision: None,
            source: DeploymentSource::Poller,
            target: DigestSet::new(),
            provenance: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownStack(s) if s == "ghost"));

    let err = store
        .transition(DeploymentId::from(999), DeploymentState::Live, None)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownDeployment(_)));
    assert!(
        store
            .assign_generation(DeploymentId::from(999))
            .await
            .is_err()
    );
    assert_eq!(
        store.deployment(DeploymentId::from(999)).await.unwrap(),
        None
    );

    // A removed stack takes no new deployments.
    store.add_revision("blog", "x").await.unwrap();
    store.remove_stack("blog").await.unwrap();
    let err = store
        .create_deployment(&NewDeployment {
            stack: "blog".into(),
            revision: None,
            source: DeploymentSource::Poller,
            target: DigestSet::new(),
            provenance: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownStack(s) if s == "blog"));
}

#[tokio::test]
async fn unfinished_and_listed_deployments() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "x").await.unwrap();
    let a = deployment(&store, "blog").await;
    let b = deployment(&store, "blog").await;
    store
        .transition(a.id, DeploymentState::Live, None)
        .await
        .unwrap();

    let unfinished: Vec<_> = store
        .unfinished_deployments()
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(unfinished, [b.id]);

    let listed: Vec<_> = store
        .deployments("blog", 10)
        .await
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(listed, [b.id, a.id]);
    assert_eq!(store.deployments("blog", 1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn commit_writes_all_pointers_at_once() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "x").await.unwrap();
    let d1 = deployment(&store, "blog").await;
    store
        .commit(
            d1.id,
            &[pointer("api", 1, Some(20000)), pointer("worker", 1, None)],
            &[],
        )
        .await
        .unwrap();
    assert!(
        store
            .deployment(d1.id)
            .await
            .unwrap()
            .unwrap()
            .committed_at
            .is_some()
    );

    // A later deployment changes `api` only and drops `worker`; `web` is new.
    let d2 = deployment(&store, "blog").await;
    store
        .commit(
            d2.id,
            &[
                pointer("api", 2, Some(20001)),
                pointer("web", 2, Some(20002)),
            ],
            &["worker".to_string()],
        )
        .await
        .unwrap();
    assert_eq!(
        store.services("blog").await.unwrap(),
        [
            pointer("api", 2, Some(20001)),
            pointer("web", 2, Some(20002))
        ]
    );
}

#[tokio::test]
async fn pointers_keep_every_published_port() {
    let (_dir, store) = open().await;
    store.add_revision("notes", "x").await.unwrap();
    // A service routed on two container ports publishes both (ADR-0026).
    let storage = ServicePointer {
        ports: PortMap::from([(9000, 20003), (9001, 20004)]),
        ..pointer("storage", 1, None)
    };
    let d1 = deployment(&store, "notes").await;
    store
        .commit(d1.id, std::slice::from_ref(&storage), &[])
        .await
        .unwrap();
    assert_eq!(
        store.services("notes").await.unwrap(),
        std::slice::from_ref(&storage)
    );

    // The next generation's pointer replaces the port set as a whole.
    let next = ServicePointer {
        ports: PortMap::from([(9000, 20005)]),
        ..pointer("storage", 2, None)
    };
    let d2 = deployment(&store, "notes").await;
    store
        .commit(d2.id, std::slice::from_ref(&next), &[])
        .await
        .unwrap();
    assert_eq!(
        store.services("notes").await.unwrap(),
        std::slice::from_ref(&next)
    );

    // A host port can back only one service.
    let d3 = deployment(&store, "notes").await;
    let err = store
        .commit(d3.id, &[pointer("web", 3, Some(20005))], &[])
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Database(_)), "{err:?}");

    let d4 = deployment(&store, "notes").await;
    store
        .commit(d4.id, &[], &["storage".to_string()])
        .await
        .unwrap();
    assert!(store.services("notes").await.unwrap().is_empty());
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM service_ports")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn failed_commit_rolls_back_entirely() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "x").await.unwrap();
    let d1 = deployment(&store, "blog").await;
    store
        .commit(d1.id, &[pointer("api", 1, Some(20000))], &[])
        .await
        .unwrap();

    // The second pointer violates `generation > 0`: nothing of this commit may stick.
    let d2 = deployment(&store, "blog").await;
    let err = store
        .commit(
            d2.id,
            &[pointer("api", 2, Some(20001)), pointer("web", 0, None)],
            &["api".to_string()],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Database(_)), "{err:?}");
    assert_eq!(
        store.services("blog").await.unwrap(),
        [pointer("api", 1, Some(20000))]
    );
    assert_eq!(
        store.deployment(d2.id).await.unwrap().unwrap().committed_at,
        None
    );
}

#[tokio::test]
async fn ports_are_reserved_once_and_released() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "x").await.unwrap();
    let alloc = PortAllocation {
        port: 20000,
        stack: "blog".into(),
        service: "api".into(),
        container_port: 8080,
        generation: 1,
    };
    store.reserve_port(&alloc).await.unwrap();
    let err = store.reserve_port(&alloc).await.unwrap_err();
    assert!(matches!(err, StoreError::PortTaken(20000)));
    assert_eq!(
        store.port_allocations().await.unwrap(),
        std::slice::from_ref(&alloc)
    );

    assert!(store.release_port(20000).await.unwrap());
    assert!(!store.release_port(20000).await.unwrap());
    assert!(store.port_allocations().await.unwrap().is_empty());

    // Reservations need a known stack and go away with it.
    let ghost = PortAllocation {
        stack: "ghost".into(),
        ..alloc.clone()
    };
    assert!(store.reserve_port(&ghost).await.is_err());
    store.reserve_port(&alloc).await.unwrap();
    store.remove_stack("blog").await.unwrap();
    assert!(store.port_allocations().await.unwrap().is_empty());
}

#[tokio::test]
async fn removing_a_stack_keeps_its_history() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "x").await.unwrap();
    let d = deployment(&store, "blog").await;
    let generation = store.assign_generation(d.id).await.unwrap();
    store
        .commit(d.id, &[pointer("api", generation, Some(20000))], &[])
        .await
        .unwrap();

    // A deployment created before the removal gets no generation after it.
    let stale = deployment(&store, "blog").await;
    assert!(store.remove_stack("blog").await.unwrap());
    assert!(!store.remove_stack("blog").await.unwrap());
    assert!(matches!(
        store.assign_generation(stale.id).await.unwrap_err(),
        StoreError::UnknownStack(s) if s == "blog"
    ));
    // Gone as a stack: no pointers, no active revision, not listed.
    assert!(store.stack("blog").await.unwrap().is_none());
    assert!(store.stacks().await.unwrap().is_empty());
    assert!(store.latest_revision("blog").await.unwrap().is_none());
    assert!(store.services("blog").await.unwrap().is_empty());
    // The history stays.
    assert_eq!(store.revisions("blog").await.unwrap().len(), 1);
    assert_eq!(store.deployments("blog", 10).await.unwrap().len(), 2);
    assert!(!store.events(d.id).await.unwrap().is_empty());

    // Applying it again revives it; revisions and generations carry on.
    let revision = store.add_revision("blog", "y").await.unwrap();
    assert_eq!(revision.revision, 2);
    assert_eq!(store.latest_revision("blog").await.unwrap(), Some(revision));
    assert_eq!(store.stacks().await.unwrap().len(), 1);
    let again = deployment(&store, "blog").await;
    assert_eq!(
        store.assign_generation(again.id).await.unwrap(),
        generation + 1
    );
}

#[tokio::test]
async fn kv_round_trip() {
    let (_dir, store) = open().await;
    assert_eq!(store.kv_get("cursor").await.unwrap(), None);
    store.kv_set("cursor", "20000").await.unwrap();
    store.kv_set("cursor", "20001").await.unwrap();
    assert_eq!(
        store.kv_get("cursor").await.unwrap().as_deref(),
        Some("20001")
    );
    assert!(store.kv_remove("cursor").await.unwrap());
    assert!(!store.kv_remove("cursor").await.unwrap());
}

/// Deploy tasks of different stacks persist their transitions in parallel (ADR-0050), next to
/// other writers. A transaction that reads before it writes must not fail with
/// `SQLITE_BUSY_SNAPSHOT` when another connection commits in between.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_do_not_conflict() {
    let (_dir, store) = open().await;
    let mut ids = Vec::new();
    for stack in ["a", "b", "c", "d"] {
        store.add_revision(stack, "x").await.unwrap();
        ids.push(deployment(&store, stack).await.id);
    }
    let tasks: Vec<_> = ids
        .into_iter()
        .enumerate()
        .map(|(n, id)| {
            let store = store.clone();
            tokio::spawn(async move {
                for i in 0..50 {
                    store
                        .transition(id, DeploymentState::Pulling, None)
                        .await
                        .unwrap();
                    store
                        .commit(id, &[pointer("api", 1, None)], &[])
                        .await
                        .unwrap();
                    store
                        .kv_set(&format!("k{n}"), &i.to_string())
                        .await
                        .unwrap();
                }
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
}

async fn integrity(path: &Path) -> Vec<String> {
    let options = SqliteConnectOptions::new().filename(path).read_only(true);
    let mut conn = sqlx::ConnectOptions::connect(&options).await.unwrap();
    sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut conn)
        .await
        .unwrap()
}

/// `hoster backup db` (ADR-0030): snapshots taken from a second, read-only handle while the
/// daemon's store keeps writing open cleanly and pass `PRAGMA integrity_check`; a snapshot
/// restored as `hoster.db` opens as a store with the data in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_while_writing_is_consistent_and_restorable() {
    let (dir, store) = open().await;
    let state = dir.path().join("state");
    store.add_revision("app", "x").await.unwrap();
    store.kv_set("before", "1").await.unwrap();

    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let writer = {
        let store = store.clone();
        tokio::spawn(async move {
            let mut started = Some(started_tx);
            for i in 0..300 {
                store
                    .kv_set(&format!("k{}", i % 10), &i.to_string())
                    .await
                    .unwrap();
                if i % 50 == 0 {
                    store.add_revision("app", &format!("v{i}")).await.unwrap();
                }
                if i == 20 {
                    let _ = started.take().unwrap().send(());
                }
            }
        })
    };

    // Snapshot only once the writer is under way, and keep going while it writes.
    let backup = Store::open_read_only(&state).await.unwrap();
    started_rx.await.unwrap();
    let mut snapshots = Vec::new();
    let mut during_writes = 0;
    while snapshots.len() < 5 || (!writer.is_finished() && snapshots.len() < 50) {
        let dest = dir.path().join(format!("snap-{}.db", snapshots.len()));
        let writing = !writer.is_finished();
        backup.snapshot(&dest).await.unwrap();
        during_writes += usize::from(writing && !writer.is_finished());
        snapshots.push(dest);
    }
    writer.await.unwrap();
    backup.close().await;
    assert!(during_writes > 0, "no snapshot overlapped the writes");

    for snap in &snapshots {
        assert_eq!(integrity(snap).await, ["ok"], "{}", snap.display());
        assert_eq!(mode(snap), 0o600);
    }
    // No temporary files are left behind.
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Restore: the snapshot becomes `hoster.db` of a fresh state dir.
    let restored = dir.path().join("restored");
    fs::create_dir(&restored).unwrap();
    fs::copy(snapshots.last().unwrap(), restored.join(Store::FILE_NAME)).unwrap();
    let store = Store::open(&restored).await.unwrap();
    assert_eq!(store.kv_get("before").await.unwrap().as_deref(), Some("1"));
    assert!(store.latest_revision("app").await.unwrap().is_some());
}

/// With hoster stopped (no `-wal` file) the read-only handle still snapshots, and the snapshot
/// is a self-contained rollback-journal file.
#[tokio::test]
async fn snapshot_of_a_stopped_database() {
    let (dir, store) = open().await;
    let state = dir.path().join("state");
    store.kv_set("a", "1").await.unwrap();
    store.close().await;
    assert!(!state.join("hoster.db-wal").exists());

    let backup = Store::open_read_only(&state).await.unwrap();
    let dest = dir.path().join("snap.db");
    backup.snapshot(&dest).await.unwrap();
    backup.close().await;

    assert_eq!(integrity(&dest).await, ["ok"]);
    // Header bytes 18/19 are the file format versions: 1 is rollback journal, 2 is WAL.
    let header = fs::read(&dest).unwrap();
    assert_eq!(header[18..20], [1, 1]);
    let restored = dir.path().join("restored");
    fs::create_dir(&restored).unwrap();
    fs::copy(&dest, restored.join(Store::FILE_NAME)).unwrap();
    let store = Store::open(&restored).await.unwrap();
    assert_eq!(store.kv_get("a").await.unwrap().as_deref(), Some("1"));
}

#[tokio::test]
async fn snapshot_refuses_to_overwrite_and_needs_an_existing_database() {
    let (dir, store) = open().await;
    let dest = dir.path().join("snap.db");
    fs::write(&dest, "keep me").unwrap();
    let err = store.snapshot(&dest).await.unwrap_err();
    assert!(matches!(err, StoreError::SnapshotExists(_)), "{err}");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "keep me");

    let err = Store::open_read_only(&dir.path().join("nowhere"))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Missing(_)), "{err}");
}

/// A target that appears while the snapshot is being written is not replaced either.
#[test]
fn publishing_a_snapshot_never_replaces_the_target() {
    let dir = TempDir::new().unwrap();
    let tmp = dir.path().join(".snap.db.tmp-1");
    let dest = dir.path().join("snap.db");
    fs::write(&tmp, "snapshot").unwrap();
    fs::write(&dest, "keep me").unwrap();
    let err = Store::publish_snapshot(&tmp, &dest).unwrap_err();
    assert!(matches!(err, StoreError::SnapshotExists(_)), "{err}");
    assert_eq!(fs::read_to_string(&dest).unwrap(), "keep me");

    fs::remove_file(&dest).unwrap();
    Store::publish_snapshot(&tmp, &dest).unwrap();
    assert_eq!(fs::read_to_string(&dest).unwrap(), "snapshot");
    assert!(!tmp.exists());
}

/// The temporary file is absolute, so SQLite never reads a relative `file:...` target as a URI.
#[test]
fn snapshot_temp_path_is_hidden_absolute_and_beside_the_target() {
    let tmp = Store::snapshot_tmp_path(Path::new("file:backups/snap.db")).unwrap();
    assert!(tmp.is_absolute(), "{}", tmp.display());
    assert!(tmp.parent().unwrap().ends_with("file:backups"));
    let name = tmp.file_name().unwrap().to_str().unwrap();
    assert_eq!(name, format!(".snap.db.tmp-{}", std::process::id()));
}

/// When `VACUUM INTO` itself fails, the temporary file it was given is removed again.
#[tokio::test]
async fn failed_snapshot_leaves_nothing_behind() {
    let (dir, store) = open().await;
    let dest = dir.path().join("missing-dir").join("snap.db");
    assert!(store.snapshot(&dest).await.is_err());
    assert!(!dest.exists());

    store.close().await;
    let dest = dir.path().join("snap.db");
    let err = store.snapshot(&dest).await.unwrap_err();
    assert!(matches!(err, StoreError::Snapshot { .. }), "{err}");
    let left: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|name| name != "state")
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn deployment_events_are_written_to_the_log_too() {
    use crate::logging::LogFormat;
    use crate::logging::capture::subscriber;

    let (_dir, store) = open().await;
    store.add_revision("blog", "x").await.unwrap();
    let (buffer, subscriber) = subscriber(LogFormat::Json, "hoster::deploy=info");
    // A current-thread test runtime: everything below logs on this thread.
    let guard = tracing::subscriber::set_default(subscriber);
    let d = deployment(&store, "blog").await;
    store
        .transition(d.id, DeploymentState::Validating, None)
        .await
        .unwrap();
    store
        .transition(d.id, DeploymentState::Failed, Some("pull failed"))
        .await
        .unwrap();
    drop(guard);

    let lines = buffer.json();
    let steps: Vec<_> = lines
        .iter()
        .map(|l| {
            (
                l["level"].as_str().unwrap(),
                l["from"].as_str(),
                l["to"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            ("INFO", None, "Received"),
            ("INFO", Some("Received"), "Validating"),
            ("WARN", Some("Validating"), "Failed"),
        ]
    );
    for line in &lines {
        assert_eq!(line["target"], "hoster::deploy");
        assert_eq!(line["stack"], "blog");
        assert_eq!(line["deployment_id"], d.id.to_string());
    }
    // The event message is its own field, next to the fixed line text.
    assert_eq!(lines[2]["message"], "deployment event");
    assert_eq!(lines[2]["detail"], "pull failed");
}

#[tokio::test]
async fn webhook_deployments_keep_their_provenance() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "a = 1").await.unwrap();
    let provenance = Provenance {
        repository: "acme/blog".into(),
        sha: "0f3c".into(),
        git_ref: "refs/tags/v1".into(),
        run_id: "77".into(),
        actor: "octocat".into(),
        tag: None,
    };
    let created = store
        .create_deployment(&NewDeployment {
            stack: "blog".into(),
            revision: None,
            source: DeploymentSource::Webhook,
            target: target(&[("api", "sha256:aa")]),
            provenance: Some(provenance.clone()),
        })
        .await
        .unwrap();
    assert_eq!(created.provenance.as_ref(), Some(&provenance));
    let loaded = store.deployment(created.id).await.unwrap().unwrap();
    assert_eq!(loaded.provenance, Some(provenance));
    assert_eq!(deployment(&store, "blog").await.provenance, None);
}

#[tokio::test]
async fn token_ids_are_single_use_until_they_expire() {
    let (_dir, store) = open().await;
    let later = Timestamp::now() + Duration::from_secs(600);
    assert!(store.claim_token("jti-1", later).await.unwrap());
    assert!(!store.claim_token("jti-1", later).await.unwrap());
    assert!(store.claim_token("jti-2", later).await.unwrap());
    // An id whose token has expired is forgotten.
    let past = Timestamp::now() - Duration::from_secs(1);
    assert!(store.claim_token("old", past).await.unwrap());
    assert!(store.claim_token("old", later).await.unwrap());
}

#[tokio::test]
async fn rejections_are_kept_per_known_stack_newest_first_and_capped() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "a = 1").await.unwrap();
    let rejection = |n: u32| WebhookRejection {
        stack: "blog".into(),
        reason: format!("attempt {n}"),
        repository: "acme/blog".into(),
        repository_id: "42".into(),
        git_ref: "refs/heads/main".into(),
        environment: None,
        workflow_ref: Some("acme/blog/.github/workflows/release.yml@refs/heads/main".into()),
        job_workflow_ref: None,
        actor: "octocat".into(),
        run_id: n.to_string(),
        at: Timestamp::now(),
    };
    for n in 0..Store::KEPT_REJECTIONS + 5 {
        assert!(store.record_rejection(&rejection(n)).await.unwrap());
    }
    let kept = store.rejections("blog", 1000).await.unwrap();
    assert_eq!(kept.len(), Store::KEPT_REJECTIONS as usize);
    assert_eq!(
        kept[0],
        WebhookRejection {
            at: kept[0].at,
            ..rejection(Store::KEPT_REJECTIONS + 4)
        }
    );
    assert_eq!(kept.last().unwrap().reason, "attempt 5");
    assert_eq!(store.rejections("blog", 2).await.unwrap().len(), 2);
    let ghost = WebhookRejection {
        stack: "ghost".into(),
        ..rejection(0)
    };
    assert!(!store.record_rejection(&ghost).await.unwrap());
}
