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
async fn a_deployments_duration_runs_from_creation_to_its_end() {
    use std::time::Duration;
    let (_dir, store) = open().await;
    store.add_revision("notes", "x").await.unwrap();
    let mut d = deployment(&store, "notes").await;
    d.finished_at = Some(d.created_at + Duration::from_millis(12_345));
    assert_eq!(d.duration(), Duration::from_millis(12_345));
    // A clock that went backwards gives zero, not a panic or a huge duration.
    d.finished_at = Some(d.created_at - Duration::from_secs(5));
    assert_eq!(d.duration(), Duration::ZERO);
    // Unfinished: up to now.
    d.finished_at = None;
    d.created_at = Timestamp::now() - Duration::from_secs(60);
    assert!(d.duration() >= Duration::from_secs(60));
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
    let ups = MIGRATOR
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .count();
    assert_eq!(applied, ups as i64);
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

    // The newest that went live, however many did not since.
    let live = |store: &Store| {
        let store = store.clone();
        async move {
            store
                .latest_live_deployment("blog")
                .await
                .unwrap()
                .map(|d| d.id)
        }
    };
    assert_eq!(live(&store).await, Some(a.id));
    let c = deployment(&store, "blog").await;
    store
        .transition(c.id, DeploymentState::LiveWithWarnings, None)
        .await
        .unwrap();
    store
        .transition(b.id, DeploymentState::Failed, None)
        .await
        .unwrap();
    assert_eq!(live(&store).await, Some(c.id));
    assert_eq!(store.latest_live_deployment("shop").await.unwrap(), None);
}

#[tokio::test]
async fn deployment_outcomes_are_counted_and_timed_per_stack_and_state() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "x").await.unwrap();
    store.add_revision("shop", "x").await.unwrap();
    store.add_revision("gone", "x").await.unwrap();
    let now = Timestamp::now();
    // Finish `id` in `state`, `took` after it was received, `ago` before now.
    let finish = async |stack: &str, state: DeploymentState, took: u64, ago: u64| {
        let d = deployment(&store, stack).await;
        store.transition(d.id, state, Some("why")).await.unwrap();
        let finished = now - Duration::from_secs(ago);
        let created = finished - Duration::from_secs(took);
        sqlx::query("UPDATE deployments SET created_at = ?, finished_at = ? WHERE id = ?")
            .bind(created)
            .bind(finished)
            .bind(d.id)
            .execute(&store.pool)
            .await
            .unwrap();
    };
    finish("blog", DeploymentState::Live, 10, 60).await;
    finish("blog", DeploymentState::Live, 20, 60).await;
    finish("blog", DeploymentState::Failed, 3, 60).await;
    // Finished before `since`: not counted.
    finish("blog", DeploymentState::Live, 500, 7200).await;
    finish("shop", DeploymentState::Rejected, 0, 60).await;
    finish("gone", DeploymentState::Live, 1, 60).await;
    // Unfinished: not counted.
    deployment(&store, "blog").await;
    store.remove_stack("gone").await.unwrap();

    let outcomes = store
        .deployment_outcomes(now - Duration::from_secs(3600))
        .await
        .unwrap();
    let summary: Vec<_> = outcomes
        .iter()
        .map(|o| (o.stack.as_str(), o.state, o.count, o.mean, o.max))
        .collect();
    let secs = Duration::from_secs;
    assert_eq!(
        summary,
        [
            ("blog", DeploymentState::Failed, 1, secs(3), secs(3)),
            ("blog", DeploymentState::Live, 2, secs(15), secs(20)),
            ("shop", DeploymentState::Rejected, 1, secs(0), secs(0)),
        ]
    );
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

#[tokio::test]
async fn the_instance_name_is_set_replaced_and_cleared() {
    let (_dir, store) = open().await;
    assert_eq!(store.instance_name().await.unwrap(), None);
    let name = |raw: &str| raw.parse::<crate::instance::InstanceName>().unwrap();
    store
        .set_instance_name(Some(&name("srv-01")))
        .await
        .unwrap();
    store
        .set_instance_name(Some(&name("prod <eu>")))
        .await
        .unwrap();
    assert_eq!(
        store.instance_name().await.unwrap(),
        Some(name("prod <eu>"))
    );
    store.set_instance_name(None).await.unwrap();
    assert_eq!(store.instance_name().await.unwrap(), None);

    // A stored name that does not validate falls back to the default.
    sqlx::query("UPDATE instance_settings SET name = 'a' || char(10) || 'b'")
        .execute(&store.pool)
        .await
        .unwrap();
    assert_eq!(store.instance_name().await.unwrap(), None);
    // The table holds one row only.
    let second =
        sqlx::query("INSERT INTO instance_settings (id, name, updated_at) VALUES (2, 'x', 1)")
            .execute(&store.pool)
            .await;
    assert!(second.is_err());
}

fn mail(raw: &str) -> crate::notify::MailAddress {
    raw.parse().unwrap()
}

fn configured(enabled: bool) -> NotificationSettings {
    NotificationSettings {
        provider: crate::notify::ProviderKind::Resend,
        from: Some(mail("hoster@example.com")),
        recipient: Some(mail("ops@example.com")),
        enabled,
        also_successes: false,
    }
}

async fn log_events(store: &Store, n: usize) -> Vec<Event> {
    let mut events = Vec::new();
    for i in 0..n {
        let event =
            NewEvent::new(EventKind::DeployFailed, Severity::Error, format!("#{i}")).stack("notes");
        events.push(store.record_event(&event).await.unwrap());
    }
    events
}

#[tokio::test]
async fn notification_settings_round_trip_and_hold_one_row() {
    let (_dir, store) = open().await;
    assert_eq!(store.notification_settings().await.unwrap(), None);

    // A configuration may be saved disabled and incomplete.
    let draft = NotificationSettings {
        from: Some(mail("hoster@example.com")),
        ..NotificationSettings::default()
    };
    store.set_notification_settings(&draft).await.unwrap();
    let stored = store.notification_settings().await.unwrap().unwrap();
    assert_eq!(stored, draft);
    assert_eq!(stored.active(), None);

    let mut settings = configured(true);
    settings.also_successes = true;
    store.set_notification_settings(&settings).await.unwrap();
    let stored = store.notification_settings().await.unwrap().unwrap();
    assert_eq!(stored, settings);
    assert_eq!(
        stored.active(),
        Some((&mail("hoster@example.com"), &mail("ops@example.com")))
    );
    let raw: (String, String) =
        sqlx::query_as("SELECT provider, recipient FROM notification_settings")
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(raw, ("resend".to_owned(), "ops@example.com".to_owned()));

    // Enabled without both addresses is refused, by the store and by the table.
    let incomplete = NotificationSettings {
        recipient: None,
        ..configured(true)
    };
    assert!(matches!(
        store.set_notification_settings(&incomplete).await,
        Err(StoreError::NotificationsIncomplete)
    ));
    let unchecked = sqlx::query("UPDATE notification_settings SET recipient = NULL")
        .execute(&store.pool)
        .await;
    assert!(unchecked.is_err());
    let second = sqlx::query(
        "INSERT INTO notification_settings (id, provider, enabled, also_successes, updated_at)
         VALUES (2, 'resend', 0, 0, 1)",
    )
    .execute(&store.pool)
    .await;
    assert!(second.is_err());

    // A row that no longer validates reads as not configured.
    sqlx::query("UPDATE notification_settings SET provider = 'carrier-pigeon'")
        .execute(&store.pool)
        .await
        .unwrap();
    assert_eq!(store.notification_settings().await.unwrap(), None);
    sqlx::query("UPDATE notification_settings SET provider = 'resend', from_address = 'nope'")
        .execute(&store.pool)
        .await
        .unwrap();
    assert_eq!(store.notification_settings().await.unwrap(), None);

    store.clear_notification_settings().await.unwrap();
    assert_eq!(store.notification_settings().await.unwrap(), None);
}

#[tokio::test]
async fn the_notifier_cursor_starts_at_the_newest_event_and_never_moves_back() {
    let (_dir, store) = open().await;
    // An empty log starts at 0, and every event is after it.
    assert_eq!(store.notifier_cursor().await.unwrap(), EventId::from(0));
    let events = log_events(&store, 3).await;
    assert_eq!(store.notifier_cursor().await.unwrap(), EventId::from(0));
    let after = store.events_after(EventId::from(0), 10).await.unwrap();
    assert_eq!(after, events);
    let after = store.events_after(events[0].id, 1).await.unwrap();
    assert_eq!(after, events[1..2]);

    store
        .advance_notifier(&NotifierStep::to(events[1].id))
        .await
        .unwrap();
    assert_eq!(store.notifier_cursor().await.unwrap(), events[1].id);
    store
        .advance_notifier(&NotifierStep::to(events[0].id))
        .await
        .unwrap();
    assert_eq!(store.notifier_cursor().await.unwrap(), events[1].id);

    // Switching notifications on moves the cursor past everything logged so far, saving them
    // again while on does not, and off then on moves it again.
    store
        .set_notification_settings(&configured(true))
        .await
        .unwrap();
    assert_eq!(store.notifier_cursor().await.unwrap(), events[2].id);
    let later = log_events(&store, 2).await;
    store
        .set_notification_settings(&configured(true))
        .await
        .unwrap();
    assert_eq!(store.notifier_cursor().await.unwrap(), events[2].id);
    store
        .set_notification_settings(&configured(false))
        .await
        .unwrap();
    assert_eq!(store.notifier_cursor().await.unwrap(), events[2].id);
    store
        .set_notification_settings(&configured(true))
        .await
        .unwrap();
    assert_eq!(store.notifier_cursor().await.unwrap(), later[1].id);
}

#[tokio::test]
async fn switching_notifications_on_forgets_the_problems_mailed_before() {
    let (_dir, store) = open().await;
    let events = log_events(&store, 1).await;
    store
        .set_notification_settings(&configured(true))
        .await
        .unwrap();
    let problem = NotifiedProblem {
        key: ProblemKey::from(&events[0]),
        event: events[0].id,
        at: Timestamp::now(),
    };
    store
        .advance_notifier(&NotifierStep {
            cursor: events[0].id,
            notified: vec![problem.clone()],
            closed: Vec::new(),
        })
        .await
        .unwrap();
    // Saving while on keeps the record; off keeps it too.
    store
        .set_notification_settings(&configured(true))
        .await
        .unwrap();
    store
        .set_notification_settings(&configured(false))
        .await
        .unwrap();
    assert_eq!(store.notified_problems().await.unwrap(), [problem]);
    // On again skips the recoveries logged meanwhile, so the record goes.
    store
        .set_notification_settings(&configured(true))
        .await
        .unwrap();
    assert_eq!(store.notified_problems().await.unwrap(), []);
}

#[tokio::test]
async fn the_cursor_and_the_notified_problems_advance_together() {
    let (_dir, store) = open().await;
    let events = log_events(&store, 3).await;
    let crash = ProblemKey {
        kind: EventKind::CrashLoopEntered,
        stack: Some("notes".to_owned()),
        subject: Some("api".to_owned()),
    };
    let cert = ProblemKey {
        kind: EventKind::CertificateFailed,
        stack: None,
        subject: Some("/certificates".to_owned()),
    };
    let at = Timestamp::now();
    store
        .advance_notifier(&NotifierStep {
            cursor: events[0].id,
            notified: vec![
                NotifiedProblem {
                    key: crash.clone(),
                    event: events[0].id,
                    at,
                },
                NotifiedProblem {
                    key: cert.clone(),
                    event: events[0].id,
                    at: at + Duration::from_secs(1),
                },
            ],
            closed: Vec::new(),
        })
        .await
        .unwrap();
    let problems = store.notified_problems().await.unwrap();
    assert_eq!(
        problems.iter().map(|p| &p.key).collect::<Vec<_>>(),
        [&crash, &cert]
    );

    // A reminder updates the record, a recovery closes it, in the step that moves the cursor.
    let reminded = NotifiedProblem {
        key: crash.clone(),
        event: events[1].id,
        at: at + Duration::from_secs(86_400),
    };
    store
        .advance_notifier(&NotifierStep {
            cursor: events[2].id,
            notified: vec![reminded.clone()],
            closed: vec![cert.clone()],
        })
        .await
        .unwrap();
    assert_eq!(store.notified_problems().await.unwrap(), [reminded]);
    assert_eq!(store.notifier_cursor().await.unwrap(), events[2].id);

    // A key without a stack or subject is its own key, not the one with an empty name.
    let bare = ProblemKey {
        kind: EventKind::HosterRolledBack,
        stack: None,
        subject: None,
    };
    assert_eq!(ProblemKey::from(&events[0]).stack.as_deref(), Some("notes"));
    store
        .advance_notifier(&NotifierStep {
            cursor: events[2].id,
            notified: vec![NotifiedProblem {
                key: bare.clone(),
                event: events[2].id,
                at,
            }],
            closed: vec![crash],
        })
        .await
        .unwrap();
    let problems = store.notified_problems().await.unwrap();
    assert_eq!(
        problems.into_iter().map(|p| p.key).collect::<Vec<_>>(),
        [bare]
    );

    // A step that fails part-way leaves nothing behind: the cursor stays with the record.
    sqlx::query(
        "CREATE TRIGGER no_certs BEFORE INSERT ON notified_problems
         WHEN NEW.kind = 'certificate_failed' BEGIN SELECT RAISE(ABORT, 'no'); END",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    let more = log_events(&store, 1).await;
    let failed = store
        .advance_notifier(&NotifierStep {
            cursor: more[0].id,
            notified: vec![NotifiedProblem {
                key: cert,
                event: more[0].id,
                at,
            }],
            closed: Vec::new(),
        })
        .await;
    assert!(failed.is_err());
    assert_eq!(store.notifier_cursor().await.unwrap(), events[2].id);
    assert_eq!(store.notified_problems().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_last_delivery_is_recorded_without_moving_the_cursor() {
    let (_dir, store) = open().await;
    assert_eq!(store.last_delivery().await.unwrap(), None);
    // Recorded before the notifier ever ran, the cursor still starts at the newest event.
    let events = log_events(&store, 2).await;
    let at = Timestamp::now();
    let sent = LastDelivery {
        at,
        outcome: DeliveryOutcome::Sent {
            id: Some("4ef9a417-02e9-4d39-ad75-9611e0fcc33c".to_owned()),
        },
    };
    store.record_delivery(&sent).await.unwrap();
    assert_eq!(store.last_delivery().await.unwrap(), Some(sent));
    assert_eq!(store.notifier_cursor().await.unwrap(), events[1].id);

    store
        .advance_notifier(&NotifierStep::to(events[1].id))
        .await
        .unwrap();
    let failed = LastDelivery {
        at: at + Duration::from_secs(60),
        outcome: DeliveryOutcome::Failed {
            error: "Resend answered 401 Unauthorized (missing_api_key)".to_owned(),
        },
    };
    store.record_delivery(&failed).await.unwrap();
    assert_eq!(store.last_delivery().await.unwrap(), Some(failed));
    let unknown_id = LastDelivery {
        at: at + Duration::from_secs(120),
        outcome: DeliveryOutcome::Sent { id: None },
    };
    store.record_delivery(&unknown_id).await.unwrap();
    assert_eq!(store.last_delivery().await.unwrap(), Some(unknown_id));
    assert_eq!(store.notifier_cursor().await.unwrap(), events[1].id);
}

#[tokio::test]
async fn a_failed_delivery_event_is_stored_as_its_text() {
    let (_dir, store) = open().await;
    let event = NewEvent::new(EventKind::NotificationFailed, Severity::Warning, "not sent");
    let recorded = store.record_event(&event).await.unwrap();
    let kind: String = sqlx::query_scalar("SELECT kind FROM events")
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(kind, "notification_failed");
    let query = EventQuery {
        kinds: [EventKind::NotificationFailed].into(),
        ..EventQuery::default()
    };
    assert_eq!(store.event_page(&query).await.unwrap().events, [recorded]);
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

fn event(kind: EventKind, severity: Severity, stack: Option<&str>) -> NewEvent {
    let event = NewEvent::new(kind, severity, format!("{kind} event")).subject("api");
    match stack {
        Some(stack) => event.stack(stack).meta("abc1234"),
        None => event,
    }
}

#[tokio::test]
async fn events_are_stored_as_their_text_and_read_back() {
    let (_dir, store) = open().await;
    for kind in EventKind::ALL {
        for severity in Severity::ALL {
            let recorded = store
                .record_event(&event(kind, severity, Some("notes")))
                .await
                .unwrap();
            let (kind_text, severity_text): (String, String) =
                sqlx::query_as("SELECT kind, severity FROM events WHERE id = ?")
                    .bind(recorded.id)
                    .fetch_one(&store.pool)
                    .await
                    .unwrap();
            assert_eq!(kind_text, kind.to_string());
            assert_eq!(severity_text, severity.to_string());
        }
    }
    let unscoped = store
        .record_event(&event(EventKind::HosterStarted, Severity::Info, None))
        .await
        .unwrap();
    let page = store.event_page(&EventQuery::default()).await.unwrap();
    assert_eq!(page.events[0], unscoped);
    assert_eq!(unscoped.stack, None);
    assert_eq!(unscoped.meta, None);
    assert_eq!(unscoped.subject.as_deref(), Some("api"));
    assert_eq!(unscoped.message, "hoster_started event");
}

#[tokio::test]
async fn events_page_newest_first_and_filter() {
    let (_dir, store) = open().await;
    let at = Timestamp::now();
    let mut recorded = Vec::new();
    // Two events share every timestamp, so the cursor must tell them apart by id.
    for (i, (kind, severity, stack)) in [
        (EventKind::DeployStarted, Severity::Info, Some("notes")),
        (EventKind::DeploySucceeded, Severity::Success, Some("notes")),
        (
            EventKind::ContainerRestarted,
            Severity::Warning,
            Some("blog"),
        ),
        (EventKind::DeployFailed, Severity::Error, Some("blog")),
        (EventKind::CertificateFailed, Severity::Error, None),
        (EventKind::HosterStarted, Severity::Info, None),
    ]
    .into_iter()
    .enumerate()
    {
        let at = at + Duration::from_millis(i as u64 / 2);
        let e = store
            .record_event_at(&event(kind, severity, stack), at)
            .await
            .unwrap();
        recorded.push(e);
    }
    let newest_first: Vec<_> = recorded.iter().rev().cloned().collect();

    // Every event, paged two at a time.
    let mut query = EventQuery {
        limit: 2,
        ..EventQuery::default()
    };
    let mut seen = Vec::new();
    loop {
        let page = store.event_page(&query).await.unwrap();
        assert!(page.events.len() <= 2);
        seen.extend(page.events);
        match page.next {
            Some(next) => query.before = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, newest_first);
    // A cursor survives its text form.
    let first = store
        .event_page(&EventQuery {
            limit: 3,
            ..EventQuery::default()
        })
        .await
        .unwrap();
    let cursor: EventCursor = first.next.unwrap().to_string().parse().unwrap();
    let rest = store
        .event_page(&EventQuery {
            before: Some(cursor),
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(rest.events, newest_first[3..]);
    assert_eq!(rest.next, None);
    // A zero limit still returns a page and a way on, not what reads as the end of the log.
    let zero = store
        .event_page(&EventQuery {
            limit: 0,
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(zero.events, newest_first[..1]);
    assert!(zero.next.is_some());

    let ids = |page: EventPage| page.events.into_iter().map(|e| e.id).collect::<Vec<_>>();
    let by_stack = store
        .event_page(&EventQuery {
            stack: Some("blog".into()),
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(ids(by_stack), [recorded[3].id, recorded[2].id]);
    let by_kind = store
        .event_page(&EventQuery {
            kinds: [EventKind::DeployStarted, EventKind::HosterStarted].into(),
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(ids(by_kind), [recorded[5].id, recorded[0].id]);
    let severe = store
        .event_page(&EventQuery {
            min_severity: Severity::Warning,
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(
        ids(severe),
        [recorded[4].id, recorded[3].id, recorded[2].id]
    );
    let combined = store
        .event_page(&EventQuery {
            stack: Some("blog".into()),
            kinds: [EventKind::DeployFailed, EventKind::DeployStarted].into(),
            min_severity: Severity::Error,
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(ids(combined), [recorded[3].id]);
}

#[tokio::test]
async fn events_inside_a_transaction_are_stored_only_if_it_commits() {
    let (_dir, store) = open().await;
    let new = event(EventKind::StackApplied, Severity::Info, Some("notes"));
    let mut tx = store.begin().await.unwrap();
    Store::insert_event(&mut tx, &new, Timestamp::now())
        .await
        .unwrap();
    drop(tx);
    let page = store.event_page(&EventQuery::default()).await.unwrap();
    assert!(page.events.is_empty());
    let mut tx = store.begin().await.unwrap();
    let kept = Store::insert_event(&mut tx, &new, Timestamp::now())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let page = store.event_page(&EventQuery::default()).await.unwrap();
    assert_eq!(page.events, [kept]);
}

#[tokio::test]
async fn events_of_a_removed_stack_are_kept() {
    let (_dir, store) = open().await;
    store.add_revision("notes", "x").await.unwrap();
    store
        .record_event(&event(
            EventKind::StackRemoved,
            Severity::Info,
            Some("notes"),
        ))
        .await
        .unwrap();
    assert!(store.remove_stack("notes").await.unwrap());
    let page = store
        .event_page(&EventQuery {
            stack: Some("notes".into()),
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
}

#[tokio::test]
async fn pruning_deletes_only_events_older_than_the_cutoff() {
    let (_dir, store) = open().await;
    let cutoff = Timestamp::now();
    let new = event(EventKind::HosterStarted, Severity::Info, None);
    store
        .record_event_at(&new, cutoff - Duration::from_millis(1))
        .await
        .unwrap();
    let at_cutoff = store.record_event_at(&new, cutoff).await.unwrap();
    let after = store
        .record_event_at(&new, cutoff + Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(store.prune_events(cutoff).await.unwrap(), 1);
    let page = store.event_page(&EventQuery::default()).await.unwrap();
    assert_eq!(page.events, [after, at_cutoff]);
    assert_eq!(store.prune_events(cutoff).await.unwrap(), 0);
}

#[tokio::test]
async fn pruning_keeps_the_newest_event_so_ids_are_never_reused() {
    let (_dir, store) = open().await;
    let new = event(EventKind::HosterStarted, Severity::Info, None);
    let old = Timestamp::now() - Duration::from_secs(86_400);
    store.record_event_at(&new, old).await.unwrap();
    let newest = store.record_event_at(&new, old).await.unwrap();
    store
        .advance_notifier(&NotifierStep::to(newest.id))
        .await
        .unwrap();
    // Every event is past retention, but the newest stays, so the next id is above the cursor.
    assert_eq!(store.prune_events(Timestamp::now()).await.unwrap(), 1);
    let next = store.record_event(&new).await.unwrap();
    assert!(next.id > newest.id);
    let cursor = store.notifier_cursor().await.unwrap();
    assert_eq!(store.events_after(cursor, 10).await.unwrap(), [next]);
}

/// Traffic history (ADR-0062), with the histogram the recorder writes.
mod traffic {
    use std::ops::Range;
    use std::time::UNIX_EPOCH;

    use super::*;
    use crate::stats::Histogram;

    /// A minute-aligned time well after the epoch: 2026-11-11 10:00 UTC.
    fn hour() -> Timestamp {
        Timestamp::from(UNIX_EPOCH + Duration::from_secs(1_794_391_200))
    }

    fn minute(n: u64) -> Timestamp {
        hour() + Duration::from_secs(60 * n)
    }

    fn series(stack: &str, service: &str) -> TrafficSeries {
        TrafficSeries {
            host: format!("{stack}.example.com"),
            prefix: "/".into(),
            stack: stack.into(),
            service: service.into(),
        }
    }

    fn histogram(millis: &[u64]) -> Histogram {
        let mut histogram = Histogram::default();
        for &ms in millis {
            histogram.record(Duration::from_millis(ms));
        }
        histogram
    }

    /// `ok` successes and `errors` server errors, one of them an upstream error, with
    /// latencies of `millis`.
    fn bucket(
        start: Timestamp,
        series: TrafficSeries,
        ok: u64,
        errors: u64,
        millis: &[u64],
    ) -> TrafficBucket<Histogram> {
        TrafficBucket {
            start,
            series,
            counts: TrafficCounts {
                s2xx: ok,
                s5xx: errors,
                upstream_errors: errors.min(1),
                ..TrafficCounts::default()
            },
            latency: histogram(millis),
        }
    }

    async fn read(
        store: &Store,
        resolution: Resolution,
        span: Range<Timestamp>,
        stack: Option<&str>,
    ) -> Vec<TrafficBucket<Histogram>> {
        store.traffic(resolution, span, stack).await.unwrap()
    }

    fn everything() -> Range<Timestamp> {
        Timestamp::from(UNIX_EPOCH)..hour() + Duration::from_secs(365 * 24 * 60 * 60)
    }

    #[test]
    fn buckets_align_to_utc_minutes_and_hours() {
        let at = minute(7) + Duration::from_millis(12_345);
        assert_eq!(Resolution::Minute.bucket(at), minute(7));
        assert_eq!(Resolution::Hour.bucket(at), hour());
        assert_eq!(Resolution::Minute.bucket(minute(7)), minute(7));
        let before_epoch = Timestamp::from(UNIX_EPOCH - Duration::from_secs(30));
        assert_eq!(
            Resolution::Minute.bucket(before_epoch),
            Timestamp::from(UNIX_EPOCH - Duration::from_secs(60))
        );
    }

    #[tokio::test]
    async fn a_minute_is_written_to_its_minute_and_hour_and_read_back() {
        let (_dir, store) = open().await;
        let notes = bucket(minute(3), series("notes", "web"), 5, 1, &[3, 4, 90]);
        let admin = bucket(minute(3), series("", "admin UI"), 2, 0, &[1]);
        let unrouted = bucket(minute(3), TrafficSeries::default(), 0, 0, &[]);
        store
            .record_traffic([notes.clone(), admin.clone(), unrouted.clone()])
            .await
            .unwrap();

        // In key order: unrouted (all '') first, then by host.
        let minutes = read(&store, Resolution::Minute, everything(), None).await;
        assert_eq!(minutes, [unrouted.clone(), admin.clone(), notes.clone()]);
        let hours = read(&store, Resolution::Hour, everything(), None).await;
        let at_hour = |b: &TrafficBucket<Histogram>| TrafficBucket {
            start: hour(),
            ..b.clone()
        };
        assert_eq!(
            hours,
            [at_hour(&unrouted), at_hour(&admin), at_hour(&notes)]
        );
        assert_eq!(hours[2].latency.samples(), 3);
    }

    #[tokio::test]
    async fn traffic_history_survives_reopening_the_store() {
        let (dir, store) = open().await;
        let notes = bucket(minute(3), series("notes", "web"), 5, 1, &[3, 4, 90]);
        store.record_traffic([notes.clone()]).await.unwrap();
        store.close().await;

        // A restart: the history is still there, and the next flush adds to it.
        let store = Store::open(&dir.path().join("state")).await.unwrap();
        assert_eq!(
            read(&store, Resolution::Minute, everything(), None).await,
            std::slice::from_ref(&notes)
        );
        store
            .record_traffic([bucket(minute(4), series("notes", "web"), 1, 0, &[2])])
            .await
            .unwrap();
        let hours = read(&store, Resolution::Hour, everything(), None).await;
        assert_eq!(hours.len(), 1);
        assert_eq!(hours[0].counts.s2xx, 6);
        assert_eq!(hours[0].latency.samples(), 4);
    }

    #[tokio::test]
    async fn writing_a_bucket_again_adds_to_it() {
        let (_dir, store) = open().await;
        let first = bucket(minute(1), series("notes", "web"), 5, 1, &[3, 4]);
        // The rest of the same minute, flushed again (a shutdown, then a restart).
        let rest = bucket(
            minute(1) + Duration::from_secs(30),
            series("notes", "web"),
            2,
            0,
            &[200],
        );
        store.record_traffic([first]).await.unwrap();
        store.record_traffic([rest]).await.unwrap();

        let expected = bucket(minute(1), series("notes", "web"), 7, 1, &[3, 4, 200]);
        let minutes = read(&store, Resolution::Minute, everything(), None).await;
        assert_eq!(minutes, std::slice::from_ref(&expected));
        let hours = read(&store, Resolution::Hour, everything(), None).await;
        assert_eq!(
            hours,
            [TrafficBucket {
                start: hour(),
                ..expected
            }]
        );
    }

    #[tokio::test]
    async fn an_hour_row_is_the_sum_of_its_minutes() {
        let (_dir, store) = open().await;
        let web = series("notes", "web");
        let api = series("notes", "api");
        // Two flushes in one hour, one in the next; a flush may carry several minutes and the
        // same series twice.
        store
            .record_traffic([
                bucket(minute(0), web.clone(), 1, 0, &[1]),
                bucket(minute(1), web.clone(), 2, 1, &[10, 20, 30]),
                bucket(minute(1), web.clone(), 1, 0, &[40]),
                bucket(minute(1), api.clone(), 4, 0, &[5]),
            ])
            .await
            .unwrap();
        store
            .record_traffic([
                bucket(minute(59), web.clone(), 3, 2, &[500]),
                bucket(minute(60), web.clone(), 9, 0, &[2]),
            ])
            .await
            .unwrap();

        let minutes = read(&store, Resolution::Minute, everything(), None).await;
        assert_eq!(minutes.len(), 5, "{minutes:?}");
        let hours = read(&store, Resolution::Hour, everything(), None).await;
        assert_eq!(hours.len(), 3, "{hours:?}");
        for hour in &hours {
            let mut sum = TrafficBucket {
                start: hour.start,
                series: hour.series.clone(),
                counts: TrafficCounts::default(),
                latency: Histogram::default(),
            };
            for minute in minutes.iter().filter(|m| {
                m.series == hour.series && Resolution::Hour.bucket(m.start) == hour.start
            }) {
                sum += minute.clone();
            }
            assert_eq!(*hour, sum);
        }
        let first = hours
            .iter()
            .find(|h| h.start == hour() && h.series == web)
            .unwrap();
        assert_eq!(first.counts.s2xx, 7);
        assert_eq!(first.counts.s5xx, 3);
        assert_eq!(first.counts.upstream_errors, 2);
        assert_eq!(first.latency.samples(), 6);
    }

    #[tokio::test]
    async fn a_span_reads_its_buckets_for_one_stack_or_all() {
        let (_dir, store) = open().await;
        store
            .record_traffic([
                bucket(minute(0), series("notes", "web"), 1, 0, &[1]),
                bucket(minute(1), series("blog", "web"), 1, 0, &[1]),
                bucket(minute(2), series("notes", "web"), 1, 0, &[1]),
                bucket(minute(3), series("notes", "web"), 1, 0, &[1]),
            ])
            .await
            .unwrap();
        let starts = |buckets: Vec<TrafficBucket<Histogram>>| -> Vec<Timestamp> {
            buckets.into_iter().map(|b| b.start).collect()
        };

        // The span includes its start and excludes its end.
        let span = minute(0)..minute(3);
        assert_eq!(
            starts(read(&store, Resolution::Minute, span.clone(), None).await),
            [minute(0), minute(1), minute(2)]
        );
        assert_eq!(
            starts(read(&store, Resolution::Minute, span.clone(), Some("notes")).await),
            [minute(0), minute(2)]
        );
        assert!(
            read(&store, Resolution::Minute, span, Some("gone"))
                .await
                .is_empty()
        );
        let hours = read(&store, Resolution::Hour, hour()..minute(60), Some("blog")).await;
        assert_eq!(starts(hours), [hour()]);
        assert!(
            read(&store, Resolution::Hour, minute(1)..minute(60), None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn pruning_deletes_buckets_before_the_cutoff_only() {
        let (_dir, store) = open().await;
        let web = series("notes", "web");
        store
            .record_traffic([
                bucket(minute(0), web.clone(), 1, 0, &[1]),
                bucket(minute(1), web.clone(), 1, 0, &[1]),
                bucket(minute(2), web.clone(), 1, 0, &[1]),
                bucket(minute(60), web.clone(), 1, 0, &[1]),
            ])
            .await
            .unwrap();

        assert_eq!(
            store
                .prune_traffic(Resolution::Minute, minute(1))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .prune_traffic(Resolution::Minute, minute(1))
                .await
                .unwrap(),
            0
        );
        let minutes = read(&store, Resolution::Minute, everything(), None).await;
        assert_eq!(
            minutes[0].start,
            minute(1),
            "the bucket at the cutoff stays"
        );
        assert_eq!(minutes.len(), 3);
        // The hours are swept on their own.
        assert_eq!(
            read(&store, Resolution::Hour, everything(), None)
                .await
                .len(),
            2
        );
        assert_eq!(
            store
                .prune_traffic(Resolution::Hour, minute(60))
                .await
                .unwrap(),
            1
        );
        let hours = read(&store, Resolution::Hour, everything(), None).await;
        assert_eq!(hours.len(), 1);
        assert_eq!(hours[0].start, minute(60));
    }

    #[tokio::test]
    async fn a_removed_stacks_history_stays() {
        let (_dir, store) = open().await;
        // No foreign key to `stacks`: even the row's deletion leaves the history.
        sqlx::query(
            "INSERT INTO stacks (name, last_generation, created_at, updated_at)
             VALUES ('notes', 0, 1, 1)",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        store
            .record_traffic([bucket(minute(0), series("notes", "web"), 1, 0, &[1])])
            .await
            .unwrap();
        sqlx::query("DELETE FROM stacks WHERE name = 'notes'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(
            read(&store, Resolution::Minute, everything(), Some("notes"))
                .await
                .len(),
            1
        );
        assert_eq!(
            read(&store, Resolution::Hour, everything(), Some("notes"))
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_malformed_histogram_is_an_error() {
        let (_dir, store) = open().await;
        store
            .record_traffic([bucket(minute(0), series("notes", "web"), 1, 0, &[1])])
            .await
            .unwrap();
        sqlx::query("UPDATE traffic_minutes SET latency = x'02'")
            .execute(&store.pool)
            .await
            .unwrap();
        let err = store
            .traffic::<Histogram>(Resolution::Minute, everything(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Latency { .. }), "{err:?}");
        // A write into the bucket fails too, and leaves the hour row as it was.
        store
            .record_traffic([bucket(minute(0), series("notes", "web"), 1, 0, &[1])])
            .await
            .unwrap_err();
        let hours = read(&store, Resolution::Hour, everything(), None).await;
        assert_eq!(hours[0].counts.s2xx, 1);
    }
}

/// Every migration is reversible and every down script restores its predecessor's schema exactly
/// (ADR-0052): what lets a newer binary roll the database back for an older one. The structural
/// checks run for both databases, `hoster.db` and the log store's `logs.db` (ADR-0067).
mod migrations {
    use sqlx::migrate::{MigrationType, Migrator};

    use super::*;
    use crate::store::logs::LOG_MIGRATOR;

    /// Every migrator, named for the failure messages.
    const MIGRATORS: [(&str, &Migrator); 2] =
        [("hoster.db", &MIGRATOR), ("logs.db", &LOG_MIGRATOR)];

    /// One row of `sqlite_schema`, without `rootpage` (which depends on the file's history).
    type SchemaRow = (String, String, String, Option<String>);

    /// A fresh, empty in-memory database with the store's foreign-key setting.
    async fn fresh() -> SqlitePool {
        let options = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap()
    }

    async fn schema(pool: &SqlitePool) -> Vec<SchemaRow> {
        sqlx::query_as("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    /// The schema of a fresh database migrated up to `version` by `migrator`.
    async fn schema_at(migrator: &Migrator, version: i64) -> Vec<SchemaRow> {
        let pool = fresh().await;
        migrator.run_to(version, &pool).await.unwrap();
        schema(&pool).await
    }

    /// The state store's migration versions, oldest first.
    fn versions() -> Vec<i64> {
        versions_of(&MIGRATOR)
    }

    fn versions_of(migrator: &Migrator) -> Vec<i64> {
        let mut versions: Vec<i64> = migrator
            .iter()
            .filter(|m| m.migration_type.is_up_migration())
            .map(|m| m.version)
            .collect();
        versions.sort_unstable();
        versions
    }

    #[test]
    fn every_up_migration_has_a_down() {
        for (name, migrator) in MIGRATORS {
            assert!(!versions_of(migrator).is_empty(), "{name}");
            for migration in migrator.iter() {
                assert_ne!(
                    migration.migration_type,
                    MigrationType::Simple,
                    "{name} migration {} ({}) is not reversible; create migrations with \
                     `sqlx migrate add -r`",
                    migration.version,
                    migration.description,
                );
            }
            let downs: BTreeSet<i64> = migrator
                .iter()
                .filter(|m| m.migration_type == MigrationType::ReversibleDown)
                .map(|m| m.version)
                .collect();
            let ups: BTreeSet<i64> = versions_of(migrator).into_iter().collect();
            assert_eq!(
                ups, downs,
                "{name}: up and down migrations must pair by version"
            );
        }
    }

    #[tokio::test]
    async fn undoing_everything_and_migrating_again_round_trips() {
        for (name, migrator) in MIGRATORS {
            let pool = fresh().await;
            migrator.run(&pool).await.unwrap();
            let migrated = schema(&pool).await;
            migrator.undo(&pool, 0).await.unwrap();
            assert_eq!(schema(&pool).await, schema_at(migrator, 0).await, "{name}");
            migrator.run(&pool).await.unwrap();
            assert_eq!(schema(&pool).await, migrated, "{name}");
        }
    }

    #[tokio::test]
    async fn each_down_restores_its_predecessor_exactly() {
        for (name, migrator) in MIGRATORS {
            let versions = versions_of(migrator);
            for (i, &version) in versions.iter().enumerate() {
                let previous = if i == 0 { 0 } else { versions[i - 1] };
                let pool = fresh().await;
                migrator.run(&pool).await.unwrap();
                migrator.undo(&pool, previous).await.unwrap();
                assert_eq!(
                    schema(&pool).await,
                    schema_at(migrator, previous).await,
                    "{name}: undoing down to {previous} (through {version}) does not restore \
                     its schema"
                );
            }
        }
    }

    /// A row in every table of the latest schema, with every later-added column set, so the
    /// down scripts run against the data a real rollback would meet.
    const POPULATE: &str = "
        INSERT INTO stacks (name, last_generation, created_at, updated_at, removed_at)
            VALUES ('notes', 2, 1, 1, NULL), ('gone', 1, 1, 1, 5);
        INSERT INTO stack_revisions (id, stack, revision, toml, hash, created_at)
            VALUES (1, 'notes', 1, 'x', 'h', 1), (2, 'gone', 1, 'y', 'h', 1);
        INSERT INTO deployments (id, stack, revision_id, generation, state, source, created_at,
                updated_at, committed_at, finished_at, source_repository, source_sha,
                source_ref, source_run_id, source_actor, source_tag)
            VALUES (1, 'notes', 1, 1, 'succeeded', 'webhook', 1, 1, 1, 1, 'o/r', 'abc',
                'refs/heads/main', '42', 'me', 'v1'),
                (2, 'notes', 1, 2, 'pulling', 'ui', 2, 2, NULL, NULL, NULL, NULL, NULL, NULL,
                NULL, NULL);
        INSERT INTO deployment_targets VALUES (1, 'api', 'sha256:aa');
        INSERT INTO deployment_events (deployment_id, from_state, to_state, message, at)
            VALUES (1, NULL, 'pending', NULL, 1), (2, NULL, 'pending', NULL, 2);
        INSERT INTO services VALUES ('notes', 'api', 1, 'sha256:aa', 'cfg', 1, 1);
        INSERT INTO service_ports VALUES ('notes', 'api', 8080, 20000);
        INSERT INTO port_allocations VALUES (20000, 'notes', 'api', 8080, 1, 1);
        INSERT INTO kv VALUES ('cursor', '20000', 1);
        INSERT INTO sessions VALUES ('t', 'c', 'me', 1, 1, 1, 2);
        INSERT INTO oauth_logins VALUES ('s', 'v', 1);
        INSERT INTO revision_images VALUES (1, 'db', 'sha256:cc');
        INSERT INTO secrets VALUES ('pw', zeroblob(24), x'00', 1, 1);
        INSERT INTO webhook_tokens VALUES ('jti', 2);
        INSERT INTO webhook_rejections (stack, reason, repository, repository_id, git_ref,
                actor, run_id, at)
            VALUES ('notes', 'ref', 'o/r', '1', 'refs/heads/x', 'me', '43', 1);
        INSERT INTO images VALUES ('docker.io/library/postgres', 'sha256:cc', 1);
        INSERT INTO events (at, kind, severity, stack, subject, message, meta)
            VALUES (1, 'deploy_succeeded', 'success', 'gone', 'api', 'deployed', 'abc1234'),
                   (2, 'notification_failed', 'warning', NULL, NULL, 'not sent', NULL);
        INSERT INTO instance_settings VALUES (1, 'srv-01', 1);
        INSERT INTO notification_settings VALUES
            (1, 'resend', 'hoster@example.com', 'ops@example.com', 1, 0, 1);
        INSERT INTO notifier_state VALUES (1, 1, 1, 'id', NULL);
        INSERT INTO notified_problems VALUES ('crash_loop_entered', 'notes', 'api', 1, 1);
        INSERT INTO traffic_minutes VALUES (60000, 'notes.example.com', '/', 'notes', 'api',
            0, 0, 5, 0, 0, 1, 1, x'01');
        INSERT INTO traffic_hours VALUES (0, '', '', '', '', 1, 0, 0, 0, 0, 0, 0, x'01');
    ";

    #[tokio::test]
    async fn undoing_a_populated_database_keeps_it_consistent() {
        let (_dir, store) = open().await;
        let pool = &store.pool;
        sqlx::raw_sql(POPULATE).execute(pool).await.unwrap();
        for &version in versions().iter().rev() {
            let previous = versions()
                .into_iter()
                .rfind(|&v| v < version)
                .unwrap_or_default();
            MIGRATOR.undo(pool, previous).await.unwrap();
            assert_eq!(
                store.schema_version().await.unwrap(),
                SchemaVersion::from(previous)
            );
            // (table, rowid, parent, fkid) of each row whose parent is gone.
            let violations: Vec<(String, Option<i64>, String, i64)> =
                sqlx::query_as("PRAGMA foreign_key_check")
                    .fetch_all(pool)
                    .await
                    .unwrap();
            assert!(
                violations.is_empty(),
                "after undo to {previous}: {violations:?}"
            );
            let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
                .fetch_all(pool)
                .await
                .unwrap();
            assert_eq!(integrity, ["ok"], "after undo to {previous}");
            if previous >= 1 {
                // The rows the older schema can hold survive the rollback.
                let stacks: i64 = sqlx::query_scalar("SELECT count(*) FROM stacks")
                    .fetch_one(pool)
                    .await
                    .unwrap();
                let deployments: i64 = sqlx::query_scalar("SELECT count(*) FROM deployments")
                    .fetch_one(pool)
                    .await
                    .unwrap();
                assert_eq!((stacks, deployments), (2, 2), "after undo to {previous}");
            }
            if (8..10).contains(&previous) {
                // The older release cannot decode a kind added after it; only those rows go.
                let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM events")
                    .fetch_all(pool)
                    .await
                    .unwrap();
                assert_eq!(kinds, ["deploy_succeeded"], "after undo to {previous}");
            }
        }
        // And the emptied database migrates up again.
        MIGRATOR.run(pool).await.unwrap();
        assert_eq!(
            store.schema_version().await.unwrap(),
            Store::expected_schema_version()
        );
        store.close().await;
    }

    #[tokio::test]
    async fn schema_versions_are_reported() {
        let latest = *versions().last().unwrap();
        assert_eq!(i64::from(Store::expected_schema_version()), latest);

        let (dir, store) = open().await;
        assert_eq!(
            store.schema_version().await.unwrap(),
            Store::expected_schema_version()
        );
        MIGRATOR.undo(&store.pool, 3).await.unwrap();
        assert_eq!(
            store.schema_version().await.unwrap(),
            SchemaVersion::from(3)
        );
        let read_only = Store::open_read_only(&dir.path().join("state"))
            .await
            .unwrap();
        assert_eq!(
            read_only.schema_version().await.unwrap(),
            SchemaVersion::from(3)
        );
        read_only.close().await;
        MIGRATOR.undo(&store.pool, 0).await.unwrap();
        assert_eq!(
            store.schema_version().await.unwrap(),
            SchemaVersion::default()
        );
        store.close().await;
    }

    #[test]
    fn migrations_between_counts_the_migrations_an_undo_reverts() {
        let all = versions();
        let latest = Store::expected_schema_version();
        assert_eq!(
            Store::migrations_between(SchemaVersion::default(), latest),
            all.len()
        );
        assert_eq!(Store::migrations_between(latest, latest), 0);
        let second = SchemaVersion::from(all[1]);
        assert_eq!(Store::migrations_between(second, latest), all.len() - 2);
        assert_eq!(
            Store::migrations_between(SchemaVersion::from(all[0]), second),
            1
        );
    }

    #[tokio::test]
    async fn an_unmigrated_database_is_at_version_zero() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(Store::FILE_NAME);
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let conn = sqlx::ConnectOptions::connect(&options).await.unwrap();
        sqlx::Connection::close(conn).await.unwrap();
        let store = Store::open_read_only(dir.path()).await.unwrap();
        assert_eq!(
            store.schema_version().await.unwrap(),
            SchemaVersion::default()
        );
    }

    #[tokio::test]
    async fn undo_migrations_reverts_a_closed_database_to_the_target() {
        let (dir, store) = open().await;
        store.add_revision("notes", "x").await.unwrap();
        store.close().await;
        let state = dir.path().join("state");
        let reached = Store::undo_migrations(&state, SchemaVersion::from(5))
            .await
            .unwrap();
        assert_eq!(reached, SchemaVersion::from(5));
        let store = Store::open_read_only(&state).await.unwrap();
        assert_eq!(
            store.schema_version().await.unwrap(),
            SchemaVersion::from(5)
        );
        store.close().await;
        // Opening normally migrates it forward again, and the stack survived.
        let store = Store::open(&state).await.unwrap();
        assert!(store.stack("notes").await.unwrap().is_some());
        store.close().await;

        let missing = TempDir::new().unwrap();
        let err = Store::undo_migrations(missing.path(), SchemaVersion::from(1))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Missing(_)), "{err}");
        assert!(!missing.path().join(Store::FILE_NAME).exists());
    }
}

#[tokio::test]
async fn restore_replaces_the_database_with_a_snapshot() {
    let (dir, store) = open().await;
    let state = dir.path().join("state");
    store.kv_set("k", "before").await.unwrap();
    let snapshot = dir.path().join("snap.db");
    store.snapshot(&snapshot).await.unwrap();
    store.kv_set("k", "after").await.unwrap();
    store.close().await;

    Store::restore(&state, &snapshot).unwrap();
    assert!(snapshot.is_file(), "the snapshot is kept");
    assert_eq!(mode(&state.join(Store::FILE_NAME)), 0o600);
    assert!(!state.join("hoster.db-wal").exists());
    let store = Store::open(&state).await.unwrap();
    assert_eq!(store.kv_get("k").await.unwrap().as_deref(), Some("before"));
    store.close().await;

    let err = Store::restore(&state, &dir.path().join("nope.db")).unwrap_err();
    assert!(matches!(err, StoreError::Io { .. }), "{err}");
    let leftovers: Vec<_> = fs::read_dir(&state)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().contains(".tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn deploy_events_are_written_with_their_transition_and_commit_or_not_at_all() {
    let (_dir, store) = open().await;
    store.add_revision("notes", "x").await.unwrap();
    let d = deployment(&store, "notes").await;
    let started = event(EventKind::DeployStarted, Severity::Info, Some("notes"));
    let recorded = store
        .transition_recorded(d.id, DeploymentState::Validating, None, &started)
        .await
        .unwrap();
    assert_eq!(recorded.kind, EventKind::DeployStarted);
    let succeeded = event(EventKind::DeploySucceeded, Severity::Success, Some("notes"));
    let committed = store
        .commit_recorded(d.id, &[pointer("api", 1, None)], &[], &succeeded)
        .await
        .unwrap();
    assert_eq!(committed.kind, EventKind::DeploySucceeded);
    assert!(
        store
            .deployment(d.id)
            .await
            .unwrap()
            .unwrap()
            .committed_at
            .is_some()
    );

    // A transition or commit that fails stores no event.
    let ghost = DeploymentId::from(999);
    assert!(
        store
            .transition_recorded(ghost, DeploymentState::Failed, Some("boom"), &started)
            .await
            .is_err()
    );
    assert!(
        store
            .commit_recorded(ghost, &[pointer("api", 2, None)], &[], &succeeded)
            .await
            .is_err()
    );
    let page = store.event_page(&EventQuery::default()).await.unwrap();
    assert_eq!(page.events, [committed, recorded]);
}

#[tokio::test]
async fn the_deploy_log_lists_a_stacks_events_in_time_order_across_deployments() {
    let (_dir, store) = open().await;
    store.add_revision("blog", "x").await.unwrap();
    store.add_revision("shop", "x").await.unwrap();
    let now = Timestamp::now();
    let mins = |m: u64| now - Duration::from_secs(m * 60);
    // A deployment of `stack` received `created` minutes ago, its later events `step`
    // minutes apart, finished with the last unless `running`.
    let seed =
        async |stack: &str, created: u64, step: u64, to: &[DeploymentState], running: bool| {
            let d = deployment(&store, stack).await;
            for state in to {
                store.transition(d.id, *state, Some("why")).await.unwrap();
            }
            let ids: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM deployment_events WHERE deployment_id = ? ORDER BY id",
            )
            .bind(d.id)
            .fetch_all(&store.pool)
            .await
            .unwrap();
            for (n, id) in ids.iter().enumerate() {
                sqlx::query("UPDATE deployment_events SET at = ? WHERE id = ?")
                    .bind(mins(created) + Duration::from_secs(n as u64 * step * 60))
                    .bind(id)
                    .execute(&store.pool)
                    .await
                    .unwrap();
            }
            let last = mins(created) + Duration::from_secs((ids.len() as u64 - 1) * step * 60);
            sqlx::query("UPDATE deployments SET created_at = ?, finished_at = ? WHERE id = ?")
                .bind(mins(created))
                .bind((!running).then_some(last))
                .bind(d.id)
                .execute(&store.pool)
                .await
                .unwrap();
            (d.id, ids)
        };
    use DeploymentState::{Failed, Live, Pulling, Validating};
    // Ten days ago: older than the log store keeps.
    let (old, old_events) = seed("blog", 14_400, 1, &[Validating, Failed], false).await;
    // Started before the range below, still running into it.
    let (long, long_events) = seed("blog", 90, 40, &[Validating, Pulling], true).await;
    let (new, new_events) = seed("blog", 30, 5, &[Validating, Live], false).await;
    seed("shop", 20, 1, &[Validating, Live], false).await;

    let query = |since: u64, page: PageAt, limit: u32| DeploymentLogQuery {
        stack: "blog".into(),
        range: mins(since)..now + Duration::from_secs(1),
        text: None,
        page,
        limit,
    };
    let keys = |page: &DeploymentLogPage| -> Vec<(DeploymentId, i64)> {
        page.entries.iter().map(|e| (e.deployment, e.id)).collect()
    };
    let all = store
        .deployment_log(&query(20_000, PageAt::Newest, 100))
        .await
        .unwrap();
    // In time order: `long`'s events are 40 minutes apart, so they bracket `new`'s.
    assert_eq!(
        keys(&all),
        [
            (old, old_events[0]),
            (old, old_events[1]),
            (old, old_events[2]),
            (long, long_events[0]),
            (long, long_events[1]),
            (new, new_events[0]),
            (new, new_events[1]),
            (new, new_events[2]),
            (long, long_events[2]),
        ]
    );
    assert!(!all.older && !all.newer);
    assert_eq!(all.entries[2].event.to, Failed);
    assert_eq!(all.entries[2].event.message.as_deref(), Some("why"));

    // The last hour: only the events in it, `long`'s included.
    let hour = store
        .deployment_log(&query(60, PageAt::Newest, 100))
        .await
        .unwrap();
    assert_eq!(
        keys(&hour),
        [
            (long, long_events[1]),
            (new, new_events[0]),
            (new, new_events[1]),
            (new, new_events[2]),
            (long, long_events[2]),
        ]
    );

    // Paging by key, both ways.
    let newest = store
        .deployment_log(&query(20_000, PageAt::Newest, 4))
        .await
        .unwrap();
    assert_eq!(keys(&newest), keys(&all)[5..]);
    assert!(newest.older && !newest.newer);
    let older = store
        .deployment_log(&query(20_000, PageAt::Before(newest.entries[0].key()), 4))
        .await
        .unwrap();
    assert_eq!(keys(&older), keys(&all)[1..5]);
    assert!(older.older && older.newer);
    let newer = store
        .deployment_log(&query(20_000, PageAt::After(older.entries[3].key()), 4))
        .await
        .unwrap();
    assert_eq!(keys(&newer), keys(&all)[5..]);
    assert!(newer.older && !newer.newer);

    // Grep matches the message.
    let mut grep = query(20_000, PageAt::Newest, 100);
    grep.text = Some(TextFilter::substring("WHY"));
    let matched = store.deployment_log(&grep).await.unwrap();
    assert_eq!(matched.entries.len(), 6);
    grep.text = Some(TextFilter::substring("nothing"));
    assert!(
        store
            .deployment_log(&grep)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}
