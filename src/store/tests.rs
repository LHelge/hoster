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
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownStack(s) if s == "ghost"));

    store.add_revision("blog", "x").await.unwrap();
    let d = deployment(&store, "blog").await;
    store.remove_stack("blog").await.unwrap();
    let err = store
        .transition(d.id, DeploymentState::Live, None)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::UnknownDeployment(_)));
    assert!(store.assign_generation(d.id).await.is_err());
    assert_eq!(store.deployment(d.id).await.unwrap(), None);
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
async fn removing_a_stack_cascades() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "x").await.unwrap();
    let d = deployment(&store, "blog").await;
    store
        .commit(d.id, &[pointer("api", 1, None)], &[])
        .await
        .unwrap();

    assert!(store.remove_stack("blog").await.unwrap());
    assert!(!store.remove_stack("blog").await.unwrap());
    assert!(store.revisions("blog").await.unwrap().is_empty());
    assert!(store.services("blog").await.unwrap().is_empty());
    assert!(store.events(d.id).await.unwrap().is_empty());
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
