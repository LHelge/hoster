use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tempfile::TempDir;

use super::*;

async fn open() -> (TempDir, LogStore) {
    let dir = TempDir::new().unwrap();
    let store = LogStore::open(&dir.path().join("state")).await.unwrap();
    (dir, store)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

async fn journal_mode(store: &LogStore) -> String {
    sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(&store.pool)
        .await
        .unwrap()
}

async fn auto_vacuum(store: &LogStore) -> i64 {
    sqlx::query_scalar("PRAGMA auto_vacuum")
        .fetch_one(&store.pool)
        .await
        .unwrap()
}

async fn tables(store: &LogStore) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE '\\_%' ESCAPE '\\' \
         ORDER BY name",
    )
    .fetch_all(&store.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn open_creates_a_private_wal_file_with_incremental_vacuum() {
    let (dir, store) = open().await;
    let state = dir.path().join("state");
    assert_eq!(store.path(), state.join("logs.db"));
    assert_eq!(mode(&state), 0o700);
    assert_eq!(mode(store.path()), 0o600);
    assert_eq!(journal_mode(&store).await, "wal");
    // 2 is INCREMENTAL.
    assert_eq!(auto_vacuum(&store).await, 2);
    assert_eq!(
        tables(&store).await,
        ["log_cursors", "log_records", "requests"]
    );
    store.close().await;
}

#[tokio::test]
async fn it_sits_beside_the_state_store_and_reopens_with_its_data() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("state");
    let main = Store::open(&state).await.unwrap();
    let logs = LogStore::open(&state).await.unwrap();
    assert_eq!(logs.path().parent(), main.path().parent());
    sqlx::query(
        "INSERT INTO log_records (time, stack, service, stream, message) \
         VALUES (1, 'notes', 'api', 'stdout', 'hello')",
    )
    .execute(&logs.pool)
    .await
    .unwrap();
    logs.close().await;

    let logs = LogStore::open(&state).await.unwrap();
    let message: String = sqlx::query_scalar("SELECT message FROM log_records")
        .fetch_one(&logs.pool)
        .await
        .unwrap();
    assert_eq!(message, "hello");
    // Nothing of the log store is in the state database.
    let shared: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_schema WHERE name IN ('log_records', 'requests')",
    )
    .fetch_one(&main.pool)
    .await
    .unwrap();
    assert_eq!(shared, 0);
    logs.close().await;
    main.close().await;
}

#[tokio::test]
async fn open_tightens_a_loose_file_mode() {
    let (dir, store) = open().await;
    let path = store.path().to_owned();
    store.close().await;
    fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

    let store = LogStore::open(&dir.path().join("state")).await.unwrap();
    assert_eq!(mode(&path), 0o600);
    store.close().await;
}

#[tokio::test]
async fn a_file_from_a_newer_hoster_is_refused() {
    let (dir, store) = open().await;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (999999, 'from the future', 1, x'00', 0)",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    store.close().await;

    let err = LogStore::open(&dir.path().join("state")).await.unwrap_err();
    assert!(matches!(err, StoreError::Migrate { .. }), "{err:?}");
    assert!(err.to_string().contains("logs.db"), "{err}");
}

#[tokio::test]
async fn a_corrupt_file_fails_to_open_with_its_name() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("state");
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("logs.db"),
        b"definitely not a sqlite database file",
    )
    .unwrap();

    let err = LogStore::open(&state).await.unwrap_err();
    assert!(
        matches!(err, StoreError::Open { .. } | StoreError::Corrupt { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("logs.db"), "{err}");
}

#[tokio::test]
async fn a_state_backup_leaves_the_log_store_out() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("state");
    let main = Store::open(&state).await.unwrap();
    let logs = LogStore::open(&state).await.unwrap();
    sqlx::query(
        "INSERT INTO log_records (time, stack, service, stream, message) \
         VALUES (1, 'notes', 'api', 'stdout', 'hello')",
    )
    .execute(&logs.pool)
    .await
    .unwrap();

    let snapshot = dir.path().join("snapshot.db");
    main.snapshot(&snapshot).await.unwrap();
    let options = SqliteConnectOptions::new()
        .filename(&snapshot)
        .read_only(true);
    let backup = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_schema")
        .fetch_all(&backup)
        .await
        .unwrap();
    assert!(!names.is_empty());
    assert!(
        !names
            .iter()
            .any(|n| n.starts_with("log_") || n == "requests"),
        "{names:?}"
    );
    backup.close().await;
    logs.close().await;
    main.close().await;
}

#[test]
fn levels_and_streams_round_trip_through_their_text() {
    for level in Level::ALL {
        assert_eq!(level.to_string().parse::<Level>(), Ok(level));
    }
    for stream in Stream::ALL {
        assert_eq!(stream.to_string().parse::<Stream>(), Ok(stream));
    }
    assert!("WARN".parse::<Level>().is_err());
    assert!("otlp".parse::<Stream>().is_err());
    assert!(Level::Warn > Level::Info && Level::Error > Level::Warn);
}

#[test]
fn request_ids_are_32_lowercase_hex_digits() {
    let id = RequestId::from([
        0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47,
        0x36,
    ]);
    assert_eq!(id.to_string(), "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!("4bf92f3577b34da6a3ce929d0e0e4736".parse(), Ok(id));
    for bad in [
        "",
        "4bf92f3577b34da6a3ce929d0e0e473",
        "4BF92F3577B34DA6A3CE929D0E0E4736",
        "4bf92f3577b34da6a3ce929d0e0e47zz",
        "4bf92f3577b34da6a3ce929d0e0e4736a",
    ] {
        assert!(bad.parse::<RequestId>().is_err(), "{bad:?}");
    }
}
