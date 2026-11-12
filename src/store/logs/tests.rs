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

mod queries {
    use std::ops::Range;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::store::Timestamp;

    fn at(millis: u64) -> Timestamp {
        Timestamp::from(UNIX_EPOCH) + Duration::from_millis(millis)
    }

    fn everything() -> Range<Timestamp> {
        at(0)..at(1_000_000_000)
    }

    fn record(time: u64, stack: Option<&str>, service: &str, message: &str) -> LogRecord {
        LogRecord {
            time: at(time),
            stack: stack.map(str::to_owned),
            service: service.to_owned(),
            generation: stack.map(|_| 1),
            stream: Stream::Stdout,
            level: None,
            message: message.to_owned(),
            fields: None,
            cut: false,
            marker: false,
        }
    }

    async fn seed(records: Vec<LogRecord>) -> (TempDir, LogStore) {
        let (dir, store) = open().await;
        store
            .append(&LogBatch {
                records,
                ..LogBatch::default()
            })
            .await
            .unwrap();
        (dir, store)
    }

    /// A small mixed store: two stacks, hoster's own records with and without a stack.
    async fn mixed() -> (TempDir, LogStore) {
        let mut records = vec![
            record(1_000, Some("notes"), "api", "Listening on :8080"),
            record(2_000, Some("notes"), "web", "GET / 200"),
            record(3_000, Some("blog"), "app", "started"),
            record(4_000, None, "hoster", "certificate renewed"),
            record(5_000, Some("notes"), "hoster", "health check failed"),
            record(6_000, Some("notes"), "api", "request failed"),
        ];
        records[0].level = Some(Level::Info);
        records[0].fields = Some(r#"{"target":"api::serve","span":{"name":"serve"}}"#.into());
        records[3].stream = Stream::Hoster;
        records[3].level = Some(Level::Info);
        records[4].stream = Stream::Hoster;
        records[4].level = Some(Level::Warn);
        records[5].level = Some(Level::Error);
        records[5].stream = Stream::Stderr;
        records[5].fields = Some(r#"{"user":{"id":42},"tags":["a","b"],"ok":false}"#.into());
        seed(records).await
    }

    async fn messages(store: &LogStore, query: &LogQuery) -> Vec<String> {
        store
            .query(query)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.record.message)
            .collect()
    }

    #[tokio::test]
    async fn scopes_pick_a_stack_hoster_or_everything() {
        let (_dir, store) = mixed().await;
        let all = LogQuery::new(everything(), LogScope::All);
        assert_eq!(messages(&store, &all).await.len(), 6);
        let notes = LogQuery::new(everything(), LogScope::Stack("notes".into()));
        assert_eq!(
            messages(&store, &notes).await,
            [
                "Listening on :8080",
                "GET / 200",
                "health check failed",
                "request failed"
            ]
        );
        let hoster = LogQuery::new(everything(), LogScope::Hoster);
        assert_eq!(messages(&store, &hoster).await, ["certificate renewed"]);
    }

    #[tokio::test]
    async fn each_filter_narrows_alone_and_together() {
        let (_dir, store) = mixed().await;
        let base = LogQuery::new(everything(), LogScope::All);

        let mut range = base.clone();
        range.range = at(2_000)..at(4_000);
        assert_eq!(messages(&store, &range).await, ["GET / 200", "started"]);

        let mut services = base.clone();
        services.services = vec!["api".into(), "hoster".into()];
        assert_eq!(messages(&store, &services).await.len(), 4);

        // Records without a level match only the empty set.
        let mut levels = base.clone();
        levels.levels = vec![Level::Warn, Level::Error];
        assert_eq!(
            messages(&store, &levels).await,
            ["health check failed", "request failed"]
        );

        let mut streams = base.clone();
        streams.streams = vec![Stream::Hoster];
        assert_eq!(
            messages(&store, &streams).await,
            ["certificate renewed", "health check failed"]
        );

        let mut text = base.clone();
        text.text = Some(TextFilter::substring("FAILED"));
        assert_eq!(
            messages(&store, &text).await,
            ["health check failed", "request failed"]
        );

        // Text matches fields as `key=value` too.
        let mut in_fields = base.clone();
        in_fields.text = Some(TextFilter::substring("user.id=42"));
        assert_eq!(messages(&store, &in_fields).await, ["request failed"]);

        let mut regex = base.clone();
        regex.text = Some(TextFilter::regex(r"^GET /\s+\d{3}$").unwrap());
        assert_eq!(messages(&store, &regex).await, ["GET / 200"]);

        let mut fields = base.clone();
        fields.fields = vec![("span.name".into(), "serve".into())];
        assert_eq!(messages(&store, &fields).await, ["Listening on :8080"]);
        fields.fields = vec![("span.name".into(), "serv".into())];
        assert!(messages(&store, &fields).await.is_empty());

        let mut combined = base.clone();
        combined.scope = LogScope::Stack("notes".into());
        combined.services = vec!["api".into(), "web".into()];
        combined.text = Some(TextFilter::regex("(?i)failed|listening").unwrap());
        combined.levels = vec![Level::Error];
        combined.fields = vec![("ok".into(), "false".into())];
        assert_eq!(messages(&store, &combined).await, ["request failed"]);
        store.close().await;
    }

    #[tokio::test]
    async fn bad_patterns_are_errors() {
        assert!(TextFilter::regex("(unclosed").is_err());
        let err = TextFilter::regex(r"(?:\w{100}){100}").unwrap_err();
        assert!(
            err.to_string().contains("invalid regular expression"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn pages_walk_equal_times_by_id_both_ways() {
        // 25 records in one millisecond, between two others.
        let mut records = vec![record(1, Some("s"), "a", "first")];
        records.extend((0..25).map(|i| record(5, Some("s"), "a", &format!("same {i:02}"))));
        records.push(record(9, Some("s"), "a", "last"));
        let (_dir, store) = seed(records).await;

        let mut query = LogQuery::new(everything(), LogScope::Stack("s".into()));
        query.limit = 10;
        let newest = store.query(&query).await.unwrap();
        assert!(newest.older && !newest.newer && newest.scan_stopped.is_none());
        let mut seen: Vec<String> = Vec::new();
        let mut page = newest;
        loop {
            let keys: Vec<RecordKey> = page.entries.iter().map(LogEntry::key).collect();
            assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
            let mut batch: Vec<String> = page
                .entries
                .iter()
                .map(|e| e.record.message.clone())
                .collect();
            batch.append(&mut seen);
            seen = batch;
            if !page.older {
                break;
            }
            query.page = PageAt::Before(page.entries[0].key());
            page = store.query(&query).await.unwrap();
            assert!(page.newer);
        }
        let mut expected = vec!["first".to_owned()];
        expected.extend((0..25).map(|i| format!("same {i:02}")));
        expected.push("last".into());
        assert_eq!(seen, expected);

        // And forwards from the oldest.
        query.page = PageAt::After(page.entries[0].key());
        let after = store.query(&query).await.unwrap();
        assert!(after.older && after.newer);
        assert_eq!(after.entries.len(), 10);
        assert_eq!(after.entries[0].record.message, "same 00");
        query.page = PageAt::After(RecordKey {
            time: at(5),
            id: after.entries[9].id + 100,
        });
        let tail = store.query(&query).await.unwrap();
        assert_eq!(tail.entries.len(), 1);
        assert!(!tail.newer);
        store.close().await;
    }

    #[tokio::test]
    async fn the_scan_stops_at_its_cap_and_says_where() {
        let cap = u64::from(LogQuery::SCAN_CAP);
        let mut records = vec![record(1, Some("s"), "a", "the needle")];
        records.extend((0..cap + 10).map(|i| record(2 + i, Some("s"), "a", "hay")));
        let (_dir, store) = seed(records).await;

        let mut query = LogQuery::new(everything(), LogScope::Stack("s".into()));
        query.text = Some(TextFilter::substring("needle"));
        let page = store.query(&query).await.unwrap();
        assert!(page.entries.is_empty());
        let stopped = page.scan_stopped.expect("stopped early");
        assert_eq!(stopped.time, at(2 + 10));
        assert!(page.older);

        query.page = PageAt::Before(stopped);
        let older = store.query(&query).await.unwrap();
        assert_eq!(older.entries.len(), 1);
        assert_eq!(older.entries[0].record.message, "the needle");
        assert!(!older.older && older.scan_stopped.is_none());
        store.close().await;
    }

    #[tokio::test]
    async fn a_range_before_the_data_is_an_empty_page() {
        let (_dir, store) = open().await;
        assert_eq!(store.oldest_record_time().await.unwrap(), None);
        store.close().await;

        let (_dir, store) = mixed().await;
        assert_eq!(store.oldest_record_time().await.unwrap(), Some(at(1_000)));
        let query = LogQuery::new(at(0)..at(500), LogScope::All);
        let page = store.query(&query).await.unwrap();
        assert_eq!(page, LogPage::default());
        store.close().await;
    }

    #[tokio::test]
    async fn level_counts_bucket_by_time() {
        let (_dir, store) = mixed().await;
        let buckets = store
            .count_by_level(everything(), &LogScope::All, Duration::from_secs(3))
            .await
            .unwrap();
        let starts: Vec<Timestamp> = buckets.iter().map(|b| b.start).collect();
        assert_eq!(starts, [at(0), at(3_000), at(6_000)]);
        assert_eq!(buckets[0].count(Some(Level::Info)), 1);
        assert_eq!(buckets[0].count(None), 1);
        assert_eq!(buckets[1].count(None), 1);
        assert_eq!(buckets[1].count(Some(Level::Info)), 1);
        assert_eq!(buckets[1].count(Some(Level::Warn)), 1);
        assert_eq!(buckets[1].total(), 3);
        assert_eq!(buckets[2].count(Some(Level::Error)), 1);

        let notes = store
            .count_by_level(
                at(0)..at(6_000),
                &LogScope::Stack("notes".into()),
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].total(), 3);
        assert_eq!(notes[0].count(Some(Level::Error)), 0);
        store.close().await;
    }

    #[tokio::test]
    async fn queries_do_not_wait_for_a_write() {
        let (_dir, store) = mixed().await;
        let mut writer = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        sqlx::query(
            "INSERT INTO log_records (time, stack, service, stream, message) \
             VALUES (7000, 'notes', 'api', 'stdout', 'uncommitted')",
        )
        .execute(&mut *writer)
        .await
        .unwrap();
        let query = LogQuery::new(everything(), LogScope::All);
        let page = tokio::time::timeout(Duration::from_secs(1), store.query(&query))
            .await
            .expect("did not wait")
            .unwrap();
        assert_eq!(page.entries.len(), 6);
        writer.rollback().await.unwrap();
        store.close().await;
    }

    #[test]
    fn fields_flatten_to_dotted_keys() {
        let mut r = record(0, None, "hoster", "m");
        r.fields = Some(
            r#"{"target":"hoster::deploy","span":{"stack":"notes","generation":3},
                "spans":[{"name":"deploy"}],"empty":{},"none":null,"text":"a \"b\""}"#
                .into(),
        );
        let flat = r.flat_fields();
        let pairs: Vec<(&str, &str)> = flat.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            pairs,
            [
                ("empty", "{}"),
                ("none", "null"),
                ("span.generation", "3"),
                ("span.stack", "notes"),
                ("spans.0.name", "deploy"),
                ("target", "hoster::deploy"),
                ("text", "a \"b\""),
            ]
        );
        r.fields = Some("[1, 2]".into());
        assert!(r.flat_fields().is_empty());
        r.fields = None;
        assert!(r.flat_fields().is_empty());
    }

    #[test]
    fn record_keys_round_trip_through_their_text() {
        let key = RecordKey {
            time: at(1_794_391_200_123),
            id: 42,
        };
        assert_eq!(key.to_string(), "1794391200123.42");
        assert_eq!(key.to_string().parse(), Ok(key));
        for bad in ["", "12", "a.1", "1.b", "1.2.3"] {
            assert!(bad.parse::<RecordKey>().is_err(), "{bad:?}");
        }
    }
}
