use std::collections::HashSet;
use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

use super::*;
use crate::store::Timestamp;

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
        ["log_cursors", "log_records", "requests", "spans"]
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
    for kind in SpanKind::ALL {
        assert_eq!(kind.to_string().parse::<SpanKind>(), Ok(kind));
    }
    for status in SpanStatus::ALL {
        assert_eq!(status.to_string().parse::<SpanStatus>(), Ok(status));
    }
    assert!("WARN".parse::<Level>().is_err());
    assert!("OTLP".parse::<Stream>().is_err());
    assert!("SERVER".parse::<SpanKind>().is_err());
    assert!("failed".parse::<SpanStatus>().is_err());
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

#[test]
fn span_ids_are_16_lowercase_hex_digits_and_take_otlp_bytes() {
    let id = SpanId::from([0x00, 0xf0, 0x67, 0xaa, 0x0b, 0xa9, 0x02, 0xb7]);
    assert_eq!(id.to_string(), "00f067aa0ba902b7");
    assert_eq!("00f067aa0ba902b7".parse(), Ok(id));
    assert_eq!(SpanId::try_from(id.as_ref()).unwrap(), id);
    for bad in [
        "",
        "00f067aa0ba902b",
        "00F067AA0BA902B7",
        "4bf92f3577b34da6a3ce929d0e0e4736",
    ] {
        assert!(bad.parse::<SpanId>().is_err(), "{bad:?}");
    }
    assert!(SpanId::try_from(&[1u8; 16][..]).is_err());
    let trace = TraceId::try_from(&[0xab; 16][..]).unwrap();
    assert_eq!(trace.to_string(), "ab".repeat(16));
}

#[test]
fn micro_timestamps_convert_to_and_from_milliseconds() {
    let ms = Timestamp::from_millis(1_700_000_000_123);
    let us = MicroTimestamp::from(ms);
    assert_eq!(us.as_micros(), 1_700_000_000_123_000);
    assert_eq!(Timestamp::from(us + Duration::from_micros(999)), ms);
    assert_eq!(
        Timestamp::from(MicroTimestamp::from_micros(-1)),
        Timestamp::from_millis(-1)
    );
    assert_eq!(
        MicroTimestamp::from_unix_nanos(1_700_000_000_123_456_789),
        MicroTimestamp::from_micros(1_700_000_000_123_456)
    );
    let now = SystemTime::now();
    assert_eq!(
        Timestamp::from(MicroTimestamp::from(now)),
        Timestamp::from(now)
    );
}

#[tokio::test]
async fn cursors_are_upserted_with_their_place_and_deleted_once_forgotten() {
    let (_dir, store) = open().await;
    let cursor = |container: &str, time_ns, nth| LogCursor {
        container: container.to_owned(),
        time_ns,
        nth,
    };
    let batch = |cursors, forgotten: &[&str]| LogBatch {
        cursors,
        forgotten: forgotten.iter().map(|c| (*c).to_owned()).collect(),
        ..LogBatch::default()
    };
    store
        .append(&batch(vec![cursor("a", 10, 1), cursor("b", 20, 3)], &[]))
        .await
        .unwrap();
    store
        .append(&batch(vec![cursor("a", 10, 2)], &[]))
        .await
        .unwrap();
    assert_eq!(store.cursor("a").await.unwrap(), Some(cursor("a", 10, 2)));
    assert_eq!(
        store.cursors().await.unwrap(),
        [cursor("a", 10, 2), cursor("b", 20, 3)]
    );

    // Forgetting a container deletes its cursor; one never stored is no error.
    store
        .append(&batch(Vec::new(), &["b", "never"]))
        .await
        .unwrap();
    assert_eq!(store.cursor("b").await.unwrap(), None);
    assert_eq!(store.cursors().await.unwrap(), [cursor("a", 10, 2)]);
    store.close().await;
}

#[tokio::test]
async fn cursors_from_before_their_place_was_stored_take_one_line_as_read() {
    let (_dir, store) = open().await;
    LOG_MIGRATOR.undo(&store.pool, 1).await.unwrap();
    sqlx::query("INSERT INTO log_cursors (container, time_ns) VALUES ('a', 10)")
        .execute(&store.pool)
        .await
        .unwrap();
    LOG_MIGRATOR.run(&store.pool).await.unwrap();
    assert_eq!(store.cursor("a").await.unwrap().map(|c| c.nth), Some(1));
    store.close().await;
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
            trace_id: None,
            span_id: None,
            received: None,
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
            .records(query)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.record.message)
            .collect()
    }

    #[tokio::test]
    async fn an_otlp_record_reads_back_with_its_trace_and_the_time_received() {
        let mut otlp = record(2_000, Some("notes"), "api", "user signed in");
        otlp.stream = Stream::Otlp;
        otlp.level = Some(Level::Info);
        otlp.fields = Some(r#"{"user.id":42,"tags":["a","b"]}"#.into());
        otlp.trace_id = Some(TraceId::from([0x4b; 16]));
        otlp.span_id = Some(SpanId::from([0x07; 8]));
        otlp.received = Some(at(2_350));
        let (_dir, store) =
            seed(vec![record(1_000, Some("notes"), "api", "x"), otlp.clone()]).await;

        let mut query = LogQuery::new(everything(), LogScope::Stack("notes".into()));
        query.streams = vec![Stream::Otlp];
        let page = store.records(&query).await.unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].record, otlp);
        store.close().await;
    }

    #[tokio::test]
    async fn a_trace_filter_reads_one_traces_records_through_its_index() {
        let traced = |time, stack, service: &str, message: &str, trace: u8, level| {
            let mut r = record(time, stack, service, message);
            r.trace_id = Some(TraceId::from([trace; 16]));
            r.span_id = Some(SpanId::from([trace; 8]));
            r.level = level;
            r
        };
        let (_dir, store) = seed(vec![
            record(1_000, Some("notes"), "api", "outside any trace"),
            traced(2_000, Some("notes"), "api", "first", 1, Some(Level::Info)),
            traced(
                3_000,
                Some("notes"),
                "web",
                "other trace",
                2,
                Some(Level::Info),
            ),
            traced(4_000, None, "hoster", "hoster's own", 1, Some(Level::Warn)),
            traced(5_000, Some("notes"), "api", "unleveled", 1, None),
        ])
        .await;
        let mut query = LogQuery::new(everything(), LogScope::All);
        query.trace = Some(TraceId::from([1; 16]));
        assert_eq!(
            messages(&store, &query).await,
            ["first", "hoster's own", "unleveled"]
        );
        // The trace narrows the other reads of the view too.
        let services: Vec<String> = store
            .services_in(&query)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.service)
            .collect();
        assert_eq!(services, ["hoster", "api"]);
        let buckets = store
            .count_by_level(&query, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(buckets.iter().map(|b| b.total()).sum::<u64>(), 3);
        query.levels = vec![Level::Warn];
        assert_eq!(messages(&store, &query).await, ["hoster's own"]);
        assert_eq!(store.count_unleveled(&query).await.unwrap(), 1);
        // With the other filters and a stack.
        query.levels.clear();
        query.scope = LogScope::Stack("notes".into());
        query.text = Some(TextFilter::substring("UNLEVELED"));
        assert_eq!(messages(&store, &query).await, ["unleveled"]);
        // Paging and following take it as well.
        query.text = None;
        query.page = PageAt::Before(RecordKey {
            time: at(5_000),
            id: 5,
        });
        assert_eq!(messages(&store, &query).await, ["first"]);
        let tail = store.records_after(&query, 0, 5).await.unwrap();
        let followed: Vec<&str> = tail
            .entries
            .iter()
            .map(|e| e.record.message.as_str())
            .collect();
        assert_eq!(followed, ["first", "unleveled"]);
        query.trace = Some(TraceId::from([9; 16]));
        query.page = PageAt::Newest;
        assert!(messages(&store, &query).await.is_empty());

        // The page reads the trace index, not the range's, whatever the range.
        let rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(
            "EXPLAIN QUERY PLAN SELECT id FROM log_records INDEXED BY log_records_by_trace \
             WHERE time >= 0 AND time < 9 AND stack = 'notes' AND trace_id = 'x' \
             ORDER BY time DESC, id DESC LIMIT 5",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        let plan = rows.into_iter().map(|r| r.3).collect::<Vec<_>>().join("; ");
        assert!(plan.contains("log_records_by_trace (trace_id=?)"), "{plan}");
        store.close().await;
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
    async fn records_without_a_level_are_counted_by_range_scope_and_service() {
        let (_dir, store) = mixed().await;
        let mut query = LogQuery::new(everything(), LogScope::All);
        // What a level filter hides: the levels and text asked for do not narrow the count.
        query.levels = vec![Level::Error];
        query.text = Some(TextFilter::substring("nothing matches this"));
        assert_eq!(store.count_unleveled(&query).await.unwrap(), 2);
        query.scope = LogScope::Stack("notes".into());
        assert_eq!(store.count_unleveled(&query).await.unwrap(), 1);
        query.services = vec!["api".into()];
        assert_eq!(store.count_unleveled(&query).await.unwrap(), 0);
        query.services.clear();
        query.range = at(3_000)..at(7_000);
        assert_eq!(store.count_unleveled(&query).await.unwrap(), 0);
        store.close().await;
    }

    #[test]
    fn matches_are_found_on_character_boundaries() {
        let found = |filter: TextFilter, text: &str| -> Vec<String> {
            filter
                .find_all(text)
                .into_iter()
                .map(|range| text[range].to_owned())
                .collect()
        };
        assert_eq!(
            found(TextFilter::substring("ERR"), "err: Error, ERRor"),
            ["err", "Err", "ERR"]
        );
        assert_eq!(found(TextFilter::substring("aa"), "aaaaa"), ["aa", "aa"]);
        assert_eq!(found(TextFilter::substring("ö"), "Öl och öl"), ["Ö", "ö"]);
        assert_eq!(found(TextFilter::substring("é"), "café"), ["é"]);
        assert!(found(TextFilter::substring("x"), "").is_empty());
        assert_eq!(
            found(TextFilter::regex(r"\d+").unwrap(), "GET /a 200 in 12ms"),
            ["200", "12"]
        );
        // Empty matches mark nothing.
        assert!(found(TextFilter::regex("z*").unwrap(), "abc").is_empty());
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
        let newest = store.records(&query).await.unwrap();
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
            page = store.records(&query).await.unwrap();
            assert!(page.newer);
        }
        let mut expected = vec!["first".to_owned()];
        expected.extend((0..25).map(|i| format!("same {i:02}")));
        expected.push("last".into());
        assert_eq!(seen, expected);

        // And forwards from the oldest.
        query.page = PageAt::After(page.entries[0].key());
        let after = store.records(&query).await.unwrap();
        assert!(after.older && after.newer);
        assert_eq!(after.entries.len(), 10);
        assert_eq!(after.entries[0].record.message, "same 00");
        query.page = PageAt::After(RecordKey {
            time: at(5),
            id: after.entries[9].id + 100,
        });
        let tail = store.records(&query).await.unwrap();
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
        let page = store.records(&query).await.unwrap();
        assert!(page.entries.is_empty());
        let stopped = page.scan_stopped.expect("stopped early");
        assert_eq!(stopped.time, at(2 + 10));
        assert!(page.older);

        query.page = PageAt::Before(stopped);
        let older = store.records(&query).await.unwrap();
        assert_eq!(older.entries.len(), 1);
        assert_eq!(older.entries[0].record.message, "the needle");
        assert!(!older.older && older.scan_stopped.is_none());
        store.close().await;
    }

    #[tokio::test]
    async fn records_after_an_id_follow_commit_order_with_the_filters() {
        let (_dir, store) = mixed().await;
        let notes = LogQuery::new(everything(), LogScope::Stack("notes".into()));
        let tail = store.records_after(&notes, 0, 6).await.unwrap();
        let ids: Vec<i64> = tail.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, [1, 2, 5, 6]);
        assert_eq!(tail.through, 6);
        assert!(!tail.more);

        // Nothing new: the cursor stays.
        let none = store.records_after(&notes, 6, 6).await.unwrap();
        assert_eq!(
            none,
            LogTail {
                through: 6,
                ..LogTail::default()
            }
        );

        // Ids, not times: a late record stamped before the others still follows them.
        let late = record(500, Some("notes"), "api", "late");
        store
            .append(&LogBatch {
                records: vec![late],
                ..LogBatch::default()
            })
            .await
            .unwrap();
        let next = store.records_after(&notes, 6, 7).await.unwrap();
        assert_eq!(next.entries.len(), 1);
        assert_eq!(next.entries[0].record.message, "late");
        assert_eq!(next.through, 7);

        // Filters SQL applies, and those matched in Rust; the cursor passes every id examined,
        // matching or not (SQL never returns `late`, which has no level).
        let mut failed = notes.clone();
        failed.levels = vec![Level::Error, Level::Warn];
        failed.text = Some(TextFilter::substring("request"));
        let tail = store.records_after(&failed, 0, 7).await.unwrap();
        assert_eq!(tail.entries.iter().map(|e| e.id).collect::<Vec<_>>(), [6]);
        assert_eq!(tail.through, 7);
        assert!(!tail.more);

        // The range still bounds it.
        let mut recent = notes.clone();
        recent.range = at(2_000)..at(1_000_000_000);
        let tail = store.records_after(&recent, 0, 7).await.unwrap();
        assert_eq!(
            tail.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            [2, 5, 6]
        );
        store.close().await;
    }

    #[tokio::test]
    async fn records_after_an_id_stop_at_the_limit_and_resume() {
        let records = (0..5).map(|i| record(i, Some("s"), "a", "line")).collect();
        let (_dir, store) = seed(records).await;
        let mut query = LogQuery::new(everything(), LogScope::All);
        query.limit = 2;
        let mut seen = Vec::new();
        let mut cursor = 0;
        loop {
            let tail = store.records_after(&query, cursor, 5).await.unwrap();
            assert!(tail.entries.len() <= 2);
            seen.extend(tail.entries.iter().map(|e| e.id));
            cursor = tail.through;
            if !tail.more {
                break;
            }
        }
        assert_eq!(seen, [1, 2, 3, 4, 5]);

        // With a filter matched in Rust, the limit counts matches.
        query.text = Some(TextFilter::substring("line"));
        let tail = store.records_after(&query, 0, 5).await.unwrap();
        assert_eq!(tail.entries.len(), 2);
        assert_eq!(tail.through, 2);
        assert!(tail.more);
        store.close().await;
    }

    #[tokio::test]
    async fn a_quiet_scope_beside_a_busy_one_moves_its_cursor_to_the_head() {
        let busy = (0..100)
            .map(|i| record(i, Some("b"), "x", "busy"))
            .collect();
        let (_dir, store) = seed(busy).await;
        let quiet = LogQuery::new(everything(), LogScope::Stack("a".into()));
        let tail = store.records_after(&quiet, 0, 100).await.unwrap();
        assert_eq!(
            tail,
            LogTail {
                through: 100,
                ..LogTail::default()
            }
        );

        store
            .append(&LogBatch {
                records: vec![record(200, Some("a"), "y", "quiet")],
                ..LogBatch::default()
            })
            .await
            .unwrap();
        let tail = store
            .records_after(&quiet, tail.through, 101)
            .await
            .unwrap();
        assert_eq!(tail.entries.len(), 1);
        assert_eq!(tail.entries[0].record.message, "quiet");
        assert_eq!(tail.through, 101);
        assert!(!tail.more);
        store.close().await;
    }

    #[tokio::test]
    async fn one_call_examines_a_bounded_window_of_ids() {
        let cap = i64::from(LogQuery::SCAN_CAP);
        let mut records: Vec<LogRecord> = (0..cap + 10)
            .map(|i| record(i as u64, Some("b"), "x", "busy"))
            .collect();
        records.push(record(1, Some("a"), "y", "quiet"));
        let (_dir, store) = seed(records).await;
        let head = cap + 11;
        let quiet = LogQuery::new(everything(), LogScope::Stack("a".into()));
        let first = store.records_after(&quiet, 0, head).await.unwrap();
        assert!(first.entries.is_empty());
        assert_eq!(first.through, cap);
        assert!(first.more, "the head is beyond the window");
        let second = store
            .records_after(&quiet, first.through, head)
            .await
            .unwrap();
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.through, head);
        assert!(!second.more);
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
        let page = store.records(&query).await.unwrap();
        assert_eq!(page, LogPage::default());
        store.close().await;
    }

    #[tokio::test]
    async fn services_in_a_range_list_their_generations() {
        let (_dir, store) = mixed().await;
        let mut replaced = record(7_000, Some("notes"), "api", "started again");
        replaced.generation = Some(2);
        store
            .append(&LogBatch {
                records: vec![replaced],
                ..LogBatch::default()
            })
            .await
            .unwrap();
        let seen = |stack: Option<&str>, service: &str, generations: &[u32]| ServiceSeen {
            stack: stack.map(str::to_owned),
            service: service.to_owned(),
            generations: generations.to_vec(),
        };
        let notes = store
            .services_in(&LogQuery::new(
                everything(),
                LogScope::Stack("notes".into()),
            ))
            .await
            .unwrap();
        assert_eq!(
            notes,
            [
                seen(Some("notes"), "api", &[1, 2]),
                seen(Some("notes"), "hoster", &[1]),
                seen(Some("notes"), "web", &[1]),
            ]
        );
        // The range narrows: api's second generation is past it.
        let early = store
            .services_in(&LogQuery::new(
                at(0)..at(6_500),
                LogScope::Stack("notes".into()),
            ))
            .await
            .unwrap();
        assert_eq!(early[0], seen(Some("notes"), "api", &[1]));
        let all = store
            .services_in(&LogQuery::new(everything(), LogScope::All))
            .await
            .unwrap();
        assert_eq!(all[0], seen(None, "hoster", &[]));
        assert_eq!(all[1], seen(Some("blog"), "app", &[1]));
        assert_eq!(all.len(), 5);
        store.close().await;
    }

    #[tokio::test]
    async fn stacks_in_a_range_are_listed_by_name_without_hosters_own() {
        let (_dir, store) = mixed().await;
        assert_eq!(
            store.stacks_in(everything(), 100).await.unwrap(),
            ["blog", "notes"]
        );
        // notes has records before and after, none inside.
        assert_eq!(
            store.stacks_in(at(2_500)..at(4_500), 100).await.unwrap(),
            ["blog"]
        );
        assert_eq!(store.stacks_in(everything(), 1).await.unwrap(), ["blog"]);
        assert!(
            store
                .stacks_in(at(0)..at(500), 100)
                .await
                .unwrap()
                .is_empty()
        );
        store.close().await;
    }

    #[tokio::test]
    async fn record_counts_take_a_scope_and_stop_at_their_cap() {
        let (_dir, store) = mixed().await;
        let count = |scope: LogScope, cap| {
            let store = store.clone();
            async move {
                store
                    .count_records(everything(), &scope, cap)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count(LogScope::All, 100).await, 6);
        assert_eq!(count(LogScope::Stack("notes".into()), 100).await, 4);
        assert_eq!(count(LogScope::Hoster, 100).await, 1);
        assert_eq!(count(LogScope::All, 2).await, 2);
        assert_eq!(
            store
                .count_records(at(0)..at(2_500), &LogScope::All, 100)
                .await
                .unwrap(),
            2
        );
        store.close().await;
    }

    #[tokio::test]
    async fn level_counts_bucket_by_time() {
        let (_dir, store) = mixed().await;
        let buckets = store
            .count_by_level(
                &LogQuery::new(everything(), LogScope::All),
                Duration::from_secs(3),
            )
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
                &LogQuery::new(at(0)..at(6_000), LogScope::Stack("notes".into())),
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].total(), 3);
        assert_eq!(notes[0].count(Some(Level::Error)), 0);

        // The service filter narrows it; a level filter does not.
        let mut query = LogQuery::new(at(0)..at(6_000), LogScope::Stack("notes".into()));
        query.services = vec!["api".into()];
        query.levels = vec![Level::Error];
        let api = store
            .count_by_level(&query, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(api.len(), 1);
        assert_eq!(api[0].total(), 1);
        assert_eq!(api[0].count(Some(Level::Info)), 1);
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
        let page = tokio::time::timeout(Duration::from_secs(1), store.records(&query))
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
        // Keys with dots or `=` stay as they are; a long or multi-line string is its text.
        r.fields = Some(r#"{"a.b":{"c=d":"x\ny"},"long":"{\"nested\":\"json\"}"}"#.into());
        assert_eq!(
            r.parsed_fields(),
            Some(Ok(vec![
                ("a.b.c=d".to_owned(), "x\ny".to_owned()),
                ("long".to_owned(), r#"{"nested":"json"}"#.to_owned()),
            ]))
        );
        // Fields that are not an object are kept raw for showing, and match nothing.
        r.fields = Some("[1, 2]".into());
        assert!(r.flat_fields().is_empty());
        assert_eq!(r.parsed_fields(), Some(Err("[1, 2]")));
        r.fields = Some("{\"cut".into());
        assert_eq!(r.parsed_fields(), Some(Err("{\"cut")));
        r.fields = None;
        assert!(r.flat_fields().is_empty());
        assert_eq!(r.parsed_fields(), None);
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

mod request_queries {
    use std::net::IpAddr;
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

    fn id(n: u8) -> RequestId {
        RequestId::from([n; 16])
    }

    fn request(time: u64, stack: &str, service: &str, path: &str, status: u16) -> RequestRow {
        RequestRow {
            time: at(time),
            host: format!("{stack}.example.com"),
            prefix: "/".into(),
            stack: stack.into(),
            service: service.into(),
            method: "GET".into(),
            path: path.into(),
            status,
            to_header: Some(Duration::from_millis(3)),
            total: Duration::from_millis(time / 100),
            bytes: 512,
            client: IpAddr::from([203, 0, 113, 7]),
            upstream_failed: false,
            // Unique per row in these tests: the time's low byte.
            request_id: id((time / 1_000) as u8),
            server_span: None,
            client_span: None,
            upstream_start: None,
        }
    }

    async fn seed(requests: Vec<RequestRow>) -> (TempDir, LogStore) {
        let (dir, store) = open().await;
        store
            .append(&LogBatch {
                requests,
                ..LogBatch::default()
            })
            .await
            .unwrap();
        (dir, store)
    }

    /// Two stacks: `notes` with `web` and `api`, `blog` with `app`.
    async fn mixed() -> (TempDir, LogStore) {
        seed(vec![
            request(1_000, "notes", "web", "/", 200),
            request(2_000, "notes", "web", "/missing", 404),
            request(3_000, "notes", "api", "/api/users/7", 500),
            request(4_000, "blog", "app", "/posts", 200),
            request(5_000, "notes", "api", "/api/users", 502),
            request(6_000, "notes", "web", "/login", 401),
        ])
        .await
    }

    async fn paths(store: &LogStore, query: &RequestQuery) -> Vec<String> {
        store
            .requests(query)
            .await
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| entry.request.path)
            .collect()
    }

    #[tokio::test]
    async fn a_row_reads_back_as_written() {
        let mut row = request(7_000, "notes", "web", "/ws", 101);
        row.client = "2001:db8::1:2:3:4".parse().unwrap();
        row.to_header = None;
        row.total = Duration::from_millis(61_000);
        row.upstream_failed = true;
        row.bytes = 1 << 40;
        row.server_span = Some(SpanId::from([0x5e; 8]));
        row.client_span = Some(SpanId::from([0xc1; 8]));
        row.upstream_start = Some(MicroTimestamp::from(at(7_000)) + Duration::from_micros(1_250));
        let (_dir, store) = seed(vec![row.clone()]).await;
        let page = store
            .requests(&RequestQuery::new(everything(), LogScope::All))
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].request, row);
        assert!(!page.older && !page.newer && page.scan_stopped.is_none());
        store.close().await;
    }

    #[tokio::test]
    async fn each_filter_narrows_alone_and_together() {
        let (_dir, store) = mixed().await;
        let all = || RequestQuery::new(everything(), LogScope::All);
        assert_eq!(paths(&store, &all()).await.len(), 6);

        let mut q = RequestQuery::new(everything(), LogScope::Stack("blog".into()));
        assert_eq!(paths(&store, &q).await, ["/posts"]);
        q.scope = LogScope::Hoster;
        assert!(paths(&store, &q).await.is_empty(), "hoster has no requests");

        let mut q = all();
        q.range = at(2_000)..at(5_000);
        assert_eq!(
            paths(&store, &q).await,
            ["/missing", "/api/users/7", "/posts"]
        );

        let mut q = all();
        q.services = vec!["api".into(), "app".into()];
        assert_eq!(
            paths(&store, &q).await,
            ["/api/users/7", "/posts", "/api/users"]
        );

        let mut q = all();
        q.status = Some(StatusFilter::Class(5));
        assert_eq!(paths(&store, &q).await, ["/api/users/7", "/api/users"]);
        q.status = Some(StatusFilter::Code(404));
        assert_eq!(paths(&store, &q).await, ["/missing"]);

        let mut q = all();
        q.min_total = Some(Duration::from_millis(50));
        assert_eq!(paths(&store, &q).await, ["/api/users", "/login"]);

        let mut q = all();
        q.path = Some(TextFilter::substring("/API/users"));
        assert_eq!(paths(&store, &q).await, ["/api/users/7", "/api/users"]);
        q.path = Some(TextFilter::regex(r"^/api/users/\d+$").unwrap());
        assert_eq!(paths(&store, &q).await, ["/api/users/7"]);

        let mut q = all();
        q.request_id = Some(id(4));
        assert_eq!(paths(&store, &q).await, ["/posts"]);

        // Together: notes' 4xx and 5xx of `web` or `api` under /api that took 30 ms or more.
        let mut q = RequestQuery::new(everything(), LogScope::Stack("notes".into()));
        q.services = vec!["web".into(), "api".into()];
        q.status = Some(StatusFilter::Class(5));
        q.min_total = Some(Duration::from_millis(30));
        q.path = Some(TextFilter::substring("/api"));
        assert_eq!(paths(&store, &q).await, ["/api/users/7", "/api/users"]);
        store.close().await;
    }

    #[test]
    fn status_filters_round_trip_through_their_text() {
        for (text, filter) in [
            ("1xx", StatusFilter::Class(1)),
            ("5xx", StatusFilter::Class(5)),
            ("404", StatusFilter::Code(404)),
            ("0", StatusFilter::Code(0)),
        ] {
            assert_eq!(text.parse(), Ok(filter), "{text}");
            assert_eq!(filter.to_string(), text);
        }
        assert_eq!("4XX".parse(), Ok(StatusFilter::Class(4)));
        for bad in ["", "6xx", "0xx", "44xx", "x", "1000", "-1", "+4", "4 04"] {
            assert!(bad.parse::<StatusFilter>().is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn counts_cover_the_range_scope_and_services() {
        let (_dir, store) = mixed().await;
        let mut q = RequestQuery::new(everything(), LogScope::Stack("notes".into()));
        // The page's other filters do not change what the footer counts.
        q.status = Some(StatusFilter::Code(404));
        let counts = store.request_counts(&q).await.unwrap();
        assert_eq!(
            counts,
            RequestCounts {
                rows: 5,
                client_errors: 2,
                server_errors: 2,
            }
        );
        q.services = vec!["web".into()];
        q.range = at(0)..at(2_500);
        let counts = store.request_counts(&q).await.unwrap();
        assert_eq!(
            (counts.rows, counts.client_errors, counts.server_errors),
            (2, 1, 0)
        );
        q.range = at(0)..at(500);
        assert_eq!(
            store.request_counts(&q).await.unwrap(),
            RequestCounts::default()
        );
        store.close().await;
    }

    /// The access log's follow stream reads by id in bounded windows, as records are followed.
    #[tokio::test]
    async fn rows_after_an_id_follow_commit_order_with_the_filters() {
        let (_dir, store) = mixed().await;
        let notes = RequestQuery::new(everything(), LogScope::Stack("notes".into()));
        let tail = store.requests_after(&notes, 0, 6).await.unwrap();
        let paths: Vec<&str> = tail
            .entries
            .iter()
            .map(|e| e.request.path.as_str())
            .collect();
        assert_eq!(
            paths,
            ["/", "/missing", "/api/users/7", "/api/users", "/login"]
        );
        assert_eq!((tail.through, tail.more), (6, false));

        // Committed late but stamped early: it still follows, by id.
        store
            .append(&LogBatch {
                requests: vec![request(500, "notes", "web", "/late", 503)],
                ..LogBatch::default()
            })
            .await
            .unwrap();
        let mut failed = notes.clone();
        failed.status = Some(StatusFilter::Class(5));
        failed.path = Some(TextFilter::substring("late"));
        let tail = store.requests_after(&failed, 6, 7).await.unwrap();
        assert_eq!(tail.entries.len(), 1);
        assert_eq!(tail.entries[0].request.path, "/late");
        assert_eq!(tail.through, 7);

        // Nothing past the head is read, and the limit leaves more for the next call.
        let mut one = notes.clone();
        one.limit = 1;
        let tail = store.requests_after(&one, 0, 3).await.unwrap();
        assert_eq!(tail.entries.len(), 1);
        assert_eq!((tail.through, tail.more), (1, true));
        let none = store.requests_after(&notes, 7, 7).await.unwrap();
        assert!(none.entries.is_empty() && none.through == 7 && !none.more);
        store.close().await;
    }

    #[tokio::test]
    async fn a_request_is_found_by_its_id() {
        let (_dir, store) = mixed().await;
        let found = store.request(id(3)).await.unwrap().expect("stored");
        assert_eq!(found.request.path, "/api/users/7");
        assert_eq!(found.request.request_id, id(3));
        assert_eq!(store.request(id(99)).await.unwrap(), None);

        // The lookup uses its index rather than scanning the table.
        let plan: Vec<(i64, i64, i64, String)> =
            sqlx::query_as("EXPLAIN QUERY PLAN SELECT id FROM requests WHERE request_id = 'x'")
                .fetch_all(store.pool())
                .await
                .unwrap();
        assert!(
            plan.iter()
                .any(|row| row.3.contains("requests_by_request_id")),
            "{plan:?}"
        );
        store.close().await;
    }

    #[tokio::test]
    async fn traced_requests_are_those_whose_rows_carry_trace_context() {
        let mut traced = request(1_000, "notes", "web", "/", 200);
        traced.server_span = Some(SpanId::from([0x5e; 8]));
        let older = request(2_000, "notes", "web", "/old", 200);
        let (_dir, store) = seed(vec![traced, older]).await;
        let found = store
            .traced_requests(&[id(1), id(2), id(99)])
            .await
            .unwrap();
        assert_eq!(found, HashSet::from([id(1)]));
        assert!(store.traced_requests(&[]).await.unwrap().is_empty());
        store.close().await;
    }

    #[tokio::test]
    async fn pages_walk_both_ways_with_the_shared_flags() {
        let rows: Vec<RequestRow> = (1..=7)
            .map(|n| request(n * 1_000, "s", "a", &format!("/{n}"), 200))
            .collect();
        let (_dir, store) = seed(rows).await;
        let mut q = RequestQuery::new(everything(), LogScope::Stack("s".into()));
        q.limit = 3;
        let newest = store.requests(&q).await.unwrap();
        assert_eq!(
            newest
                .entries
                .iter()
                .map(|e| e.request.path.as_str())
                .collect::<Vec<_>>(),
            ["/5", "/6", "/7"]
        );
        assert!(newest.older && !newest.newer);

        q.page = PageAt::Before(newest.entries[0].key());
        let older = store.requests(&q).await.unwrap();
        assert_eq!(older.entries.len(), 3);
        assert_eq!(older.entries[0].request.path, "/2");
        assert!(older.older && older.newer);

        q.page = PageAt::After(older.entries[0].key());
        let newer = store.requests(&q).await.unwrap();
        assert_eq!(newer.entries[0].request.path, "/3");
        assert!(newer.older && newer.newer);
        q.page = PageAt::After(newest.entries[0].key());
        let tail = store.requests(&q).await.unwrap();
        assert_eq!(tail.entries.len(), 2);
        assert!(!tail.newer);
        store.close().await;
    }

    #[tokio::test]
    async fn a_path_scan_stops_at_the_shared_cap() {
        let cap = u64::from(LogQuery::SCAN_CAP);
        let mut rows = vec![request(1, "s", "a", "/needle", 200)];
        rows.extend((0..cap + 10).map(|i| {
            let mut row = request(2 + i, "s", "a", "/hay", 200);
            row.request_id = RequestId::from((u128::from(i) + 1).to_be_bytes());
            row
        }));
        let (_dir, store) = seed(rows).await;

        let mut q = RequestQuery::new(everything(), LogScope::Stack("s".into()));
        q.path = Some(TextFilter::substring("needle"));
        let page = store.requests(&q).await.unwrap();
        assert!(page.entries.is_empty());
        let stopped = page.scan_stopped.expect("stopped early");
        assert_eq!(stopped.time, at(2 + 10));

        q.page = PageAt::Before(stopped);
        let older = store.requests(&q).await.unwrap();
        assert_eq!(older.entries.len(), 1);
        assert_eq!(older.entries[0].request.path, "/needle");
        assert!(older.scan_stopped.is_none());
        store.close().await;
    }
}

mod retention {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use super::*;
    use crate::store::Timestamp;

    const HOUR: Duration = Duration::from_secs(60 * 60);
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// A fixed "now", half past an hour, so whole hours lie before it.
    fn now() -> Timestamp {
        Timestamp::from_millis(1_800_000_000_000).floor(HOUR) + HOUR / 2
    }

    fn record(time: Timestamp, bytes: usize) -> LogRecord {
        LogRecord {
            time,
            stack: Some("notes".to_owned()),
            service: "api".to_owned(),
            generation: Some(1),
            stream: Stream::Stdout,
            level: None,
            message: "x".repeat(bytes),
            fields: None,
            cut: false,
            marker: false,
            trace_id: None,
            span_id: None,
            received: None,
        }
    }

    fn request(time: Timestamp, bytes: usize) -> RequestRow {
        RequestRow {
            time,
            host: "notes.example.com".to_owned(),
            prefix: "/".to_owned(),
            stack: "notes".to_owned(),
            service: "api".to_owned(),
            method: "GET".to_owned(),
            path: "/".repeat(bytes.max(1)),
            status: 200,
            to_header: None,
            total: Duration::from_millis(3),
            bytes: 10,
            client: IpAddr::V4(Ipv4Addr::LOCALHOST),
            upstream_failed: false,
            request_id: RequestId::from([1; 16]),
            server_span: None,
            client_span: None,
            upstream_start: None,
        }
    }

    fn span(time: Timestamp, bytes: usize) -> Span {
        Span {
            trace_id: TraceId::from([1; 16]),
            span_id: SpanId::from([2; 8]),
            parent: None,
            stack: "notes".to_owned(),
            service: "api".to_owned(),
            name: "x".repeat(bytes),
            kind: SpanKind::Server,
            start: MicroTimestamp::from(time) + Duration::from_micros(250),
            duration: Duration::from_micros(40),
            status: SpanStatus::Unset,
            status_message: None,
            attributes: None,
            resource: None,
            events: None,
            received: time,
        }
    }

    /// Appends `records` and `requests` rows of `bytes` each at `time`, and as many spans as
    /// request rows, started within that millisecond.
    async fn seed(
        store: &LogStore,
        time: Timestamp,
        records: usize,
        requests: usize,
        bytes: usize,
    ) {
        store
            .append(&LogBatch {
                records: (0..records).map(|_| record(time, bytes)).collect(),
                requests: (0..requests).map(|_| request(time, bytes)).collect(),
                spans: (0..requests).map(|_| span(time, bytes)).collect(),
                ..LogBatch::default()
            })
            .await
            .unwrap();
    }

    /// The times of the stored records and request rows, oldest first.
    async fn times(store: &LogStore) -> (Vec<i64>, Vec<i64>) {
        let records = sqlx::query_scalar("SELECT time FROM log_records ORDER BY time, id")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        let requests = sqlx::query_scalar("SELECT time FROM requests ORDER BY time, id")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        (records, requests)
    }

    /// The starts of the stored spans, in milliseconds, oldest first.
    async fn span_times(store: &LogStore) -> Vec<i64> {
        sqlx::query_scalar("SELECT start / 1000 FROM spans ORDER BY start, id")
            .fetch_all(&store.pool)
            .await
            .unwrap()
    }

    async fn used_bytes(store: &LogStore) -> u64 {
        store.file_size().await.unwrap().used()
    }

    fn ms(time: Timestamp) -> i64 {
        time.as_millis()
    }

    #[tokio::test]
    async fn rows_older_than_the_age_go_from_both_tables() {
        let (_dir, store) = open().await;
        let old = now() - 8 * DAY;
        let recent = now() - DAY;
        seed(&store, old, 3, 2, 10).await;
        seed(&store, recent, 1, 1, 10).await;

        let report = store
            .sweep(now(), Retention::default())
            .finish()
            .await
            .unwrap();
        assert_eq!(report.started, now());
        assert_eq!(
            report.expired,
            Removed {
                records: 3,
                requests: 2,
                spans: 2
            }
        );
        assert_eq!(report.capped, Removed::default());
        assert!(report.file_bytes > 0);
        assert_eq!(times(&store).await, (vec![ms(recent)], vec![ms(recent)]));
        assert_eq!(span_times(&store).await, [ms(recent)]);
    }

    #[tokio::test]
    async fn the_newest_row_of_each_table_stays_however_old() {
        let (_dir, store) = open().await;
        seed(&store, now() - 30 * DAY, 2, 2, 10).await;
        let head = store.head().await.unwrap();

        let tiny = Retention::new(DAY, 1, 100);
        let report = store.sweep(now(), tiny).finish().await.unwrap();
        assert_eq!(
            report.expired,
            Removed {
                records: 1,
                requests: 1,
                spans: 1
            }
        );
        assert_eq!(report.capped, Removed::default());
        let old = ms(now() - 30 * DAY);
        assert_eq!(times(&store).await, (vec![old], vec![old]));
        assert_eq!(span_times(&store).await, [old]);
        // The next id is not one handed out before.
        assert_eq!(store.head().await.unwrap(), head);
    }

    #[tokio::test]
    async fn spans_go_after_three_days_while_records_and_requests_stay_seven() {
        let (_dir, store) = open().await;
        let four_days = now() - 4 * DAY;
        let two_days = now() - 2 * DAY;
        seed(&store, four_days, 2, 2, 10).await;
        seed(&store, two_days, 1, 1, 10).await;

        let report = store
            .sweep(now(), Retention::default())
            .finish()
            .await
            .unwrap();
        assert_eq!(
            report.expired,
            Removed {
                spans: 2,
                ..Removed::default()
            }
        );
        assert_eq!(span_times(&store).await, [ms(two_days)]);
        let (records, requests) = times(&store).await;
        assert_eq!((records.len(), requests.len()), (3, 3));
    }

    #[tokio::test]
    async fn deletes_go_in_bounded_batches() {
        let (_dir, store) = open().await;
        seed(&store, now() - 8 * DAY, 5, 0, 10).await;
        seed(&store, now(), 1, 0, 10).await;

        let mut sweep = store.sweep(now(), Retention::new(Retention::MAX_AGE, u64::MAX, 2));
        assert_eq!(sweep.step().await.unwrap(), None);
        assert_eq!(times(&store).await.0.len(), 4);
        assert_eq!(sweep.step().await.unwrap(), None);
        assert_eq!(times(&store).await.0.len(), 2);
        let report = sweep.finish().await.unwrap();
        assert_eq!(report.expired.records, 5);
        assert_eq!(times(&store).await.0, [ms(now())]);
    }

    #[tokio::test]
    async fn over_the_cap_whole_hours_go_oldest_first_across_both_tables() {
        let (_dir, store) = open().await;
        let first = now().floor(HOUR) - 3 * HOUR;
        let second = first + HOUR;
        // The hour's records and requests at its start and end.
        seed(&store, first, 20, 0, 8_000).await;
        seed(
            &store,
            first + HOUR - Duration::from_millis(1),
            0,
            20,
            8_000,
        )
        .await;
        seed(&store, second, 20, 20, 8_000).await;
        seed(&store, now(), 1, 1, 10).await;
        let used = used_bytes(&store).await;

        // Room for a little more than one of the hours.
        let cap = used * 3 / 5;
        let report = store
            .sweep(now(), Retention::new(Retention::MAX_AGE, cap, 7))
            .finish()
            .await
            .unwrap();
        assert_eq!(report.expired, Removed::default());
        assert_eq!(
            report.capped,
            Removed {
                records: 20,
                requests: 20,
                spans: 20
            }
        );
        let (records, requests) = times(&store).await;
        let spans = span_times(&store).await;
        assert!(
            records
                .iter()
                .chain(&requests)
                .chain(&spans)
                .all(|&t| t >= ms(second))
        );
        assert_eq!((records.len(), requests.len(), spans.len()), (21, 21, 21));
        assert!(used_bytes(&store).await <= cap);
    }

    #[tokio::test]
    async fn the_hour_in_progress_stays_even_over_the_cap() {
        let (_dir, store) = open().await;
        let earlier = now().floor(HOUR) - HOUR;
        seed(&store, earlier, 5, 5, 4_000).await;
        // A log storm in the current hour, alone larger than the cap.
        seed(&store, now().floor(HOUR), 50, 50, 4_000).await;

        let report = store
            .sweep(now(), Retention::new(Retention::MAX_AGE, 1, 100))
            .finish()
            .await
            .unwrap();
        assert_eq!(
            report.capped,
            Removed {
                records: 5,
                requests: 5,
                spans: 5
            }
        );
        let (records, requests) = times(&store).await;
        assert_eq!((records.len(), requests.len()), (50, 50));
        assert_eq!(span_times(&store).await.len(), 50);
    }

    #[tokio::test]
    async fn the_vacuum_shrinks_the_file_on_disk() {
        let (_dir, store) = open().await;
        seed(&store, now() - 8 * DAY, 500, 500, 4_000).await;
        seed(&store, now(), 1, 1, 10).await;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&store.pool)
            .await
            .unwrap();
        let before = fs::metadata(store.path()).unwrap().len();

        let report = store
            .sweep(now(), Retention::default())
            .finish()
            .await
            .unwrap();
        assert_eq!(
            report.expired,
            Removed {
                records: 500,
                requests: 500,
                spans: 500
            }
        );
        let after = fs::metadata(store.path()).unwrap().len();
        assert!(after < before / 10, "{after} is not much below {before}");
        assert_eq!(report.file_bytes, after);
        assert_eq!(store.file_size().await.unwrap().free, 0);
    }

    #[tokio::test]
    async fn the_last_hour_is_counted_in_both_tables_and_the_used_size_read() {
        let (_dir, store) = open().await;
        seed(&store, now() - HOUR * 2, 3, 2, 10).await;
        seed(&store, now() - HOUR / 2, 4, 1, 10).await;
        seed(&store, now(), 1, 0, 10).await;
        assert_eq!(
            store.rows_since(now() - HOUR).await.unwrap(),
            RowCounts {
                records: 5,
                requests: 1,
                spans: 1
            }
        );
        assert_eq!(
            store.rows_since(now() + HOUR).await.unwrap(),
            RowCounts::default()
        );
        assert_eq!(store.used_bytes().await.unwrap(), used_bytes(&store).await);
        assert!(store.used_bytes().await.unwrap() > 0);
    }
}

#[tokio::test]
async fn doctor_inspects_an_existing_file_read_only_and_never_creates_one() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("state");
    assert_eq!(LogStore::inspect_in(&state).await.unwrap(), None);
    assert!(!state.exists(), "inspecting created the state directory");

    LogStore::open(&state).await.unwrap().close().await;
    assert_eq!(
        LogStore::inspect_in(&state).await.unwrap(),
        Some(LogFileFacts {
            incremental_vacuum: true,
            schema: LogStore::expected_schema_version(),
        })
    );

    // A file made without the vacuum mode, as a hand-made or very old file would be.
    let plain = dir.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(plain.join(LogStore::FILE_NAME))
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    sqlx::query("CREATE TABLE t (x)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert_eq!(
        LogStore::inspect_in(&plain).await.unwrap(),
        Some(LogFileFacts {
            incremental_vacuum: false,
            schema: SchemaVersion::from(0),
        })
    );

    fs::write(state.join(LogStore::FILE_NAME), b"not a database at all").unwrap();
    for side in ["-wal", "-shm"] {
        let _ = fs::remove_file(LogStore::sidecar(&state.join(LogStore::FILE_NAME), side));
    }
    assert!(LogStore::inspect_in(&state).await.is_err());
}

#[tokio::test]
async fn rollback_reads_and_undoes_the_log_schema() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("state");
    assert_eq!(LogStore::schema_version_in(&state).await.unwrap(), None);
    assert!(matches!(
        LogStore::undo_migrations(&state, SchemaVersion::from(1))
            .await
            .unwrap_err(),
        StoreError::Missing(_)
    ));
    assert!(!state.join(LogStore::FILE_NAME).exists(), "never created");

    LogStore::open(&state).await.unwrap().close().await;
    let expected = LogStore::expected_schema_version();
    assert_eq!(
        LogStore::schema_version_in(&state).await.unwrap(),
        Some(expected)
    );
    assert_eq!(
        LogStore::migrations_between(SchemaVersion::from(1), expected),
        usize::try_from(i64::from(expected)).unwrap() - 1
    );
    assert_eq!(LogStore::migrations_between(expected, expected), 0);

    let reached = LogStore::undo_migrations(&state, SchemaVersion::from(1))
        .await
        .unwrap();
    assert_eq!(reached, SchemaVersion::from(1));
    assert_eq!(
        LogStore::schema_version_in(&state).await.unwrap(),
        Some(SchemaVersion::from(1))
    );
    // The older schema opens and migrates forward again.
    LogStore::open(&state).await.unwrap().close().await;
    assert_eq!(
        LogStore::schema_version_in(&state).await.unwrap(),
        Some(expected)
    );
}

#[tokio::test]
async fn moving_the_log_store_aside_takes_its_sidecars_and_drops_stale_ones() {
    let dir = TempDir::new().unwrap();
    let state = dir.path().join("state");
    LogStore::open(&state).await.unwrap().close().await;
    let file = |name: &str| state.join(name);
    fs::write(file("logs.db-wal"), b"wal").unwrap();
    // Left from an earlier move aside: it does not belong to the file moved now.
    fs::write(file("logs.db.rollback-v0.2.0-shm"), b"stale").unwrap();

    let aside = LogStore::move_aside(&state, "rollback-v0.2.0").unwrap();
    assert_eq!(aside, file("logs.db.rollback-v0.2.0"));
    assert!(aside.is_file());
    assert_eq!(
        fs::read(file("logs.db.rollback-v0.2.0-wal")).unwrap(),
        b"wal"
    );
    for gone in [
        "logs.db",
        "logs.db-wal",
        "logs.db-shm",
        "logs.db.rollback-v0.2.0-shm",
    ] {
        assert!(!file(gone).exists(), "{gone}");
    }
    assert!(matches!(
        LogStore::move_aside(&state, "rollback-v0.2.0").unwrap_err(),
        StoreError::Io { .. }
    ));
}

mod spans {
    use std::ops::Range;

    use super::*;

    const HOUR: Duration = Duration::from_secs(60 * 60);

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    fn everything() -> Range<Timestamp> {
        at(0)..at(1_000_000_000)
    }

    fn trace(n: u8) -> TraceId {
        TraceId::from([n; 16])
    }

    /// A span of `trace` started `start_us` µs after the epoch.
    fn span(trace: TraceId, id: u8, parent: Option<u8>, stack: &str, start_us: i64) -> Span {
        Span {
            trace_id: trace,
            span_id: SpanId::from([id; 8]),
            parent: parent.map(|p| SpanId::from([p; 8])),
            stack: stack.to_owned(),
            service: "api".to_owned(),
            name: format!("span {id}"),
            kind: SpanKind::Internal,
            start: MicroTimestamp::from_micros(start_us),
            duration: Duration::from_micros(130),
            status: SpanStatus::Unset,
            status_message: None,
            attributes: None,
            resource: None,
            events: None,
            received: at(900_000),
        }
    }

    async fn seed(spans: Vec<Span>) -> (TempDir, LogStore) {
        let (dir, store) = open().await;
        store
            .append(&LogBatch {
                spans,
                ..LogBatch::default()
            })
            .await
            .unwrap();
        (dir, store)
    }

    fn ids(spans: &[Span]) -> Vec<String> {
        spans
            .iter()
            .map(|s| s.span_id.to_string()[..2].to_owned())
            .collect()
    }

    #[tokio::test]
    async fn a_span_reads_back_as_written() {
        let mut full = span(trace(1), 1, None, "notes", 1_234_567);
        full.kind = SpanKind::Server;
        full.status = SpanStatus::Error;
        full.status_message = Some("upstream timed out".into());
        full.attributes = Some(r#"{"http.route":"/users/:id","retries":[1,2]}"#.into());
        full.resource = Some(r#"{"service.name":"notes-api"}"#.into());
        full.events = Some(r#"[{"name":"exception","time_us":1234600}]"#.into());
        full.duration = Duration::from_micros(7);
        let child = span(trace(1), 2, Some(1), "notes", 1_234_570);
        let (_dir, store) = seed(vec![child.clone(), full.clone()]).await;

        assert_eq!(store.trace_spans(trace(1)).await.unwrap(), [full, child]);
        store.close().await;
    }

    #[tokio::test]
    async fn a_trace_is_its_spans_by_start() {
        let (_dir, store) = seed(vec![
            span(trace(1), 3, Some(1), "notes", 3_000),
            span(trace(2), 9, None, "notes", 1_500),
            span(trace(1), 1, None, "notes", 1_000),
            span(trace(1), 2, Some(1), "blog", 2_000),
        ])
        .await;
        assert_eq!(
            ids(&store.trace_spans(trace(1)).await.unwrap()),
            ["01", "02", "03"]
        );
        assert!(store.trace_spans(trace(7)).await.unwrap().is_empty());
        store.close().await;
    }

    #[tokio::test]
    async fn roots_over_a_range_and_scope_come_newest_first() {
        let ms = 1_000;
        let (_dir, store) = seed(vec![
            span(trace(1), 1, None, "notes", 1_000 * ms),
            span(trace(1), 2, Some(1), "notes", 1_001 * ms),
            span(trace(2), 3, None, "blog", 2_000 * ms),
            span(trace(3), 4, None, "notes", 3_000 * ms),
            // Its parent was lost or is the proxy's: not a root here.
            span(trace(4), 5, Some(99), "notes", 4_000 * ms),
        ])
        .await;
        let roots = |query: RootQuery| {
            let store = store.clone();
            async move { ids(&store.root_spans(&query).await.unwrap()) }
        };

        assert_eq!(
            roots(RootQuery::new(everything(), LogScope::All)).await,
            ["04", "03", "01"]
        );
        assert_eq!(
            roots(RootQuery::new(
                everything(),
                LogScope::Stack("notes".into())
            ))
            .await,
            ["04", "01"]
        );
        assert!(
            roots(RootQuery::new(everything(), LogScope::Hoster))
                .await
                .is_empty()
        );
        // The range is in milliseconds, its end exclusive.
        assert_eq!(
            roots(RootQuery::new(at(1_000)..at(3_000), LogScope::All)).await,
            ["03", "01"]
        );
        let mut limited = RootQuery::new(everything(), LogScope::All);
        limited.limit = 2;
        assert_eq!(roots(limited).await, ["04", "03"]);
        store.close().await;
    }

    #[tokio::test]
    async fn trace_and_root_reads_use_their_indexes() {
        let (_dir, store) = open().await;
        let plan = |sql: &'static str| {
            let store = store.clone();
            async move {
                let rows: Vec<(i64, i64, i64, String)> =
                    sqlx::query_as(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")))
                        .fetch_all(&store.pool)
                        .await
                        .unwrap();
                rows.into_iter().map(|r| r.3).collect::<Vec<_>>().join("; ")
            }
        };
        let by_trace = plan("SELECT * FROM spans WHERE trace_id = 'x' ORDER BY start").await;
        assert!(by_trace.contains("spans_by_trace"), "{by_trace}");
        let roots = plan(
            "SELECT * FROM spans WHERE parent_span_id IS NULL AND start >= 0 AND start < 9 \
             ORDER BY start DESC LIMIT 5",
        )
        .await;
        assert!(roots.contains("spans_roots_by_start"), "{roots}");
        let stack_roots = plan(
            "SELECT * FROM spans WHERE parent_span_id IS NULL AND start >= 0 AND start < 9 \
             AND stack = 'notes' ORDER BY start DESC LIMIT 5",
        )
        .await;
        assert!(
            stack_roots.contains("spans_roots_by_stack"),
            "{stack_roots}"
        );
        store.close().await;
    }

    #[tokio::test]
    async fn spans_of_the_last_hour_are_counted_by_start() {
        let now = at(1_800_000_000_000);
        let micros = |t: Timestamp| MicroTimestamp::from(t).as_micros();
        let (_dir, store) = seed(vec![
            span(trace(1), 1, None, "notes", micros(now - 2 * HOUR)),
            span(trace(2), 2, None, "notes", micros(now - HOUR / 2)),
        ])
        .await;
        assert_eq!(store.rows_since(now - HOUR).await.unwrap().spans, 1);
        store.close().await;
    }
}

mod traces {
    use std::net::IpAddr;
    use std::ops::Range;

    use super::*;

    fn at(millis: i64) -> Timestamp {
        Timestamp::from_millis(millis)
    }

    fn everything() -> Range<Timestamp> {
        at(0)..at(1_000_000_000)
    }

    fn trace(n: u8) -> TraceId {
        TraceId::from([n; 16])
    }

    fn span_id(n: u8) -> SpanId {
        SpanId::from([n; 8])
    }

    /// A request routed at `time` ms that took `total` ms, with the proxy's span ids
    /// `0xa<n>` (server) and `0xc<n>` (client, upstream start 0.5 ms in).
    fn request(
        n: u8,
        time: i64,
        stack: &str,
        service: &str,
        status: u16,
        total: u64,
    ) -> RequestRow {
        RequestRow {
            time: at(time),
            host: format!("{stack}.example.com"),
            prefix: "/".into(),
            stack: stack.into(),
            service: service.into(),
            method: "GET".into(),
            path: format!("/{n}"),
            status,
            to_header: Some(Duration::from_millis(1)),
            total: Duration::from_millis(total),
            bytes: 1,
            client: IpAddr::from([203, 0, 113, 7]),
            upstream_failed: false,
            request_id: trace(n),
            server_span: Some(span_id(0xa0 + n)),
            client_span: Some(span_id(0xc0 + n)),
            upstream_start: Some(MicroTimestamp::from(at(time)) + Duration::from_micros(500)),
        }
    }

    /// A span of `trace` started at `start_us` µs that took `ms` milliseconds.
    #[allow(clippy::too_many_arguments)]
    fn span(
        trace: TraceId,
        id: u8,
        parent: Option<u8>,
        stack: &str,
        service: &str,
        start_us: i64,
        ms: u64,
        status: SpanStatus,
    ) -> Span {
        Span {
            trace_id: trace,
            span_id: span_id(id),
            parent: parent.map(span_id),
            stack: stack.into(),
            service: service.into(),
            name: format!("span {id}"),
            kind: SpanKind::Internal,
            start: MicroTimestamp::from_micros(start_us),
            duration: Duration::from_millis(ms),
            status,
            status_message: None,
            attributes: None,
            resource: None,
            events: None,
            received: at(900_000),
        }
    }

    fn record(time: i64, trace: TraceId, span: Option<SpanId>, message: &str) -> LogRecord {
        LogRecord {
            time: at(time),
            stack: Some("notes".into()),
            service: "web".into(),
            generation: Some(1),
            stream: Stream::Otlp,
            level: Some(Level::Info),
            message: message.into(),
            fields: None,
            cut: false,
            marker: false,
            trace_id: Some(trace),
            span_id: span,
            received: Some(at(time)),
        }
    }

    /// - 1: a request to `notes/web` (50 ms) whose app sent a root, a child in error and a
    ///   span whose parent was lost;
    /// - 2: a request to `notes/api` (900 ms), app not instrumented;
    /// - 3: a request to `blog/app` that hoster answered 502 itself (5 ms): no client span;
    /// - 4: a request row from before span ids were stored: no trace;
    /// - 5: a background job of `notes/worker` (300 ms) with two roots and a child;
    /// - 6: a request to `notes/web` (20 ms) whose app started a trace of its own id anyway.
    async fn sample() -> (TempDir, LogStore) {
        use SpanStatus::{Error, Unset};
        let (dir, store) = open().await;
        let mut three = request(3, 3_000, "blog", "app", 502, 5);
        three.upstream_failed = true;
        (three.client_span, three.upstream_start) = (None, None);
        let mut four = request(4, 4_000, "notes", "web", 200, 10);
        (four.server_span, four.client_span, four.upstream_start) = (None, None, None);
        let batch = LogBatch {
            requests: vec![
                request(1, 1_000, "notes", "web", 200, 50),
                request(2, 2_000, "notes", "api", 200, 900),
                three,
                four,
                request(6, 6_000, "notes", "web", 200, 20),
            ],
            spans: vec![
                span(
                    trace(1),
                    0x11,
                    Some(0xc1),
                    "notes",
                    "web",
                    1_001_000,
                    40,
                    Unset,
                ),
                span(
                    trace(1),
                    0x12,
                    Some(0x11),
                    "notes",
                    "web",
                    1_002_000,
                    10,
                    Error,
                ),
                span(
                    trace(1),
                    0x13,
                    Some(0xee),
                    "notes",
                    "web",
                    1_030_000,
                    1,
                    Unset,
                ),
                span(
                    trace(5),
                    0x51,
                    None,
                    "notes",
                    "worker",
                    5_000_000,
                    300,
                    Unset,
                ),
                span(
                    trace(5),
                    0x52,
                    Some(0x51),
                    "notes",
                    "worker",
                    5_001_000,
                    10,
                    Unset,
                ),
                span(trace(5), 0x53, None, "notes", "worker", 5_400_000, 1, Unset),
                span(trace(6), 0x61, None, "notes", "web", 6_001_000, 5, Unset),
            ],
            records: vec![
                record(1_003, trace(1), Some(span_id(0x11)), "second"),
                record(1_002, trace(1), Some(span_id(0x11)), "first"),
                record(1_001, trace(1), Some(span_id(0xa1)), "proxy"),
                record(1_004, trace(1), None, "no span"),
                record(2_001, trace(2), Some(span_id(0x11)), "another trace"),
            ],
            ..LogBatch::default()
        };
        store.append(&batch).await.unwrap();
        (dir, store)
    }

    fn numbers(page: &TracePage) -> Vec<u8> {
        page.traces.iter().map(|t| t.trace_id.as_ref()[0]).collect()
    }

    async fn list(store: &LogStore, query: &TraceQuery) -> Vec<u8> {
        numbers(&store.traces(query).await.unwrap())
    }

    #[tokio::test]
    async fn the_list_has_a_row_per_trace_from_request_rows_and_stored_roots() {
        let (_dir, store) = sample().await;
        let page = store
            .traces(&TraceQuery::new(everything(), LogScope::All))
            .await
            .unwrap();
        assert_eq!(numbers(&page), [6, 5, 3, 2, 1]);
        assert_eq!((page.matched, page.total, page.more), (5, 5, false));

        let row = |n: u8| page.traces.iter().find(|t| t.trace_id == trace(n)).unwrap();
        let one = row(1);
        assert_eq!(one.name, "GET /1");
        assert_eq!(one.stack, "notes");
        assert_eq!(one.services, ["hoster", "web"]);
        assert_eq!(one.spans, 5);
        assert_eq!(one.duration, Duration::from_millis(50));
        assert_eq!(one.start, MicroTimestamp::from(at(1_000)));
        assert_eq!(one.status, TraceStatus::Error, "a child span is in error");
        assert!(!one.proxy_only);

        let two = row(2);
        assert!(two.proxy_only, "app not instrumented");
        assert_eq!(
            (two.services.as_slice(), two.spans),
            (&["hoster".to_owned()][..], 2)
        );
        assert_eq!(two.status, TraceStatus::Ok);

        let three = row(3);
        assert!(three.proxy_only);
        assert_eq!((three.spans, three.status), (1, TraceStatus::Error));

        let five = row(5);
        assert_eq!(five.name, "span 81", "the earliest of its two roots");
        assert_eq!(
            (five.services.as_slice(), five.spans),
            (&["worker".to_owned()][..], 3)
        );
        assert_eq!(five.duration, Duration::from_millis(300));
        assert!(!five.proxy_only);

        let six = row(6);
        assert_eq!(
            six.name, "GET /6",
            "the request row is the root, listed once"
        );
        assert_eq!(six.services, ["hoster", "web"]);
        store.close().await;
    }

    #[tokio::test]
    async fn the_list_sorts_and_filters() {
        let (_dir, store) = sample().await;
        let all = || TraceQuery::new(everything(), LogScope::All);

        let mut q = all();
        q.sort = TraceSort::Slowest;
        assert_eq!(list(&store, &q).await, [2, 5, 1, 6, 3]);
        q.sort = TraceSort::ErrorsFirst;
        assert_eq!(list(&store, &q).await, [3, 1, 6, 5, 2]);

        let mut q = all();
        q.status = Some(TraceStatus::Error);
        assert_eq!(list(&store, &q).await, [3, 1]);
        q.status = Some(TraceStatus::Ok);
        assert_eq!(list(&store, &q).await, [6, 5, 2]);

        let mut q = all();
        q.min_duration = Some(Duration::from_millis(300));
        assert_eq!(list(&store, &q).await, [5, 2]);

        let mut q = all();
        q.scope = LogScope::Stack("blog".into());
        assert_eq!(list(&store, &q).await, [3]);
        q.scope = LogScope::Hoster;
        assert!(list(&store, &q).await.is_empty());

        let mut q = all();
        q.services = vec!["worker".into()];
        assert_eq!(list(&store, &q).await, [5]);
        q.services = vec!["api".into()];
        assert_eq!(
            list(&store, &q).await,
            [2],
            "the service a request was routed to"
        );
        q.services = vec!["web".into(), "worker".into()];
        assert_eq!(list(&store, &q).await, [6, 5, 1]);
        q.services = vec![Span::PROXY_SERVICE.into()];
        assert_eq!(list(&store, &q).await, [6, 3, 2, 1]);

        // Grep on span names, ignoring case: the root's (a request's) or any stored span's.
        let mut q = all();
        q.grep = Some("get /6".into());
        assert_eq!(list(&store, &q).await, [6]);
        q.grep = Some("SPAN 82".into());
        assert_eq!(list(&store, &q).await, [5], "a child span's name");
        q.grep = Some(String::new());
        assert_eq!(list(&store, &q).await.len(), 5, "an empty grep is none");
        assert_eq!(
            store
                .count_traces(everything(), &LogScope::All)
                .await
                .unwrap(),
            5
        );

        // The range is in milliseconds, its end exclusive.
        let q = TraceQuery::new(at(2_000)..at(5_000), LogScope::All);
        let page = store.traces(&q).await.unwrap();
        assert_eq!((numbers(&page), page.total), (vec![3, 2], 2));
        store.close().await;
    }

    #[tokio::test]
    async fn the_list_pages_and_counts_matches_apart_from_the_total() {
        let (_dir, store) = sample().await;
        let mut q = TraceQuery::new(everything(), LogScope::All);
        q.limit = 2;
        q.page = 1;
        let page = store.traces(&q).await.unwrap();
        assert_eq!(numbers(&page), [3, 2]);
        assert_eq!((page.matched, page.total, page.more), (5, 5, true));
        q.page = 2;
        let page = store.traces(&q).await.unwrap();
        assert_eq!((numbers(&page), page.more), (vec![1], false));
        q.page = 9;
        let page = store.traces(&q).await.unwrap();
        assert!(page.traces.is_empty());
        assert_eq!((page.matched, page.total), (5, 5));

        q.page = 0;
        q.status = Some(TraceStatus::Error);
        let page = store.traces(&q).await.unwrap();
        assert_eq!((page.matched, page.total), (2, 5));
        store.close().await;
    }

    #[tokio::test]
    async fn the_scatter_has_every_root_by_start() {
        let (_dir, store) = sample().await;
        let points = store
            .trace_points(&TraceQuery::new(everything(), LogScope::All))
            .await
            .unwrap();
        let got: Vec<(u8, Duration, TraceStatus)> = points
            .points
            .iter()
            .map(|p| (p.trace_id.as_ref()[0], p.duration, p.status))
            .collect();
        let ms = Duration::from_millis;
        assert_eq!(
            got,
            [
                (1, ms(50), TraceStatus::Error),
                (2, ms(900), TraceStatus::Ok),
                (3, ms(5), TraceStatus::Error),
                (5, ms(300), TraceStatus::Ok),
                (6, ms(20), TraceStatus::Ok),
            ]
        );
        assert_eq!(points.points[0].name, "GET /1");
        assert_eq!(points.matched, 5);
        assert!(!points.is_sampled());
        store.close().await;
    }

    #[tokio::test]
    async fn the_scatter_samples_evenly_over_its_cap() {
        let (_dir, store) = open().await;
        let n = 2_500;
        let requests = (0..n)
            .map(|i| {
                let mut row = request(1, 1_000 + i, "notes", "web", 200, 5);
                let mut id = [0; 16];
                id[..8].copy_from_slice(&(i as u64 + 1).to_be_bytes());
                row.request_id = TraceId::from(id);
                row
            })
            .collect();
        store
            .append(&LogBatch {
                requests,
                ..LogBatch::default()
            })
            .await
            .unwrap();
        let points = store
            .trace_points(&TraceQuery::new(everything(), LogScope::All))
            .await
            .unwrap();
        assert_eq!(points.points.len(), TraceQuery::MAX_POINTS as usize);
        assert_eq!(points.matched, n as u64);
        assert!(points.is_sampled());
        // Every 2.5th root: the gaps are 2 or 3 ms, never more.
        let starts: Vec<i64> = points.points.iter().map(|p| p.start.as_micros()).collect();
        assert_eq!(starts[0], MicroTimestamp::from(at(1_000)).as_micros());
        assert!(
            starts
                .windows(2)
                .all(|w| matches!(w[1] - w[0], 2_000 | 3_000))
        );
        store.close().await;
    }

    #[tokio::test]
    async fn one_trace_is_its_proxy_and_stored_spans_as_a_tree() {
        let (_dir, store) = sample().await;
        let trace_one = store.load_trace(trace(1)).await.unwrap().unwrap();
        let ids: Vec<SpanId> = trace_one.spans().iter().map(|s| s.span.span_id).collect();
        assert_eq!(
            ids,
            [
                span_id(0xa1),
                span_id(0xc1),
                span_id(0x11),
                span_id(0x12),
                span_id(0x13)
            ]
        );
        let depths: Vec<usize> = trace_one.spans().iter().map(|s| s.depth).collect();
        assert_eq!(depths, [0, 1, 2, 3, 1]);
        let lost = trace_one.span(span_id(0x13)).unwrap();
        assert!(
            lost.parent_missing,
            "its parent was lost: shown under the root"
        );
        assert_eq!(trace_one.total_spans(), 5);
        assert!(!trace_one.is_truncated());
        assert_eq!(trace_one.services(), ["hoster", "web"]);
        let server = &trace_one.spans()[0].span;
        assert_eq!(
            (server.kind, server.service.as_str()),
            (SpanKind::Server, "hoster")
        );
        assert_eq!(server.duration, Duration::from_millis(50));

        // Log records: per span, and the trace's own count includes those without a span.
        assert_eq!(trace_one.log_records(), 4);
        let counts: Vec<u64> = trace_one.spans().iter().map(|s| s.log_records).collect();
        assert_eq!(counts, [1, 0, 2, 0, 0]);
        let records = store
            .span_records(trace(1), span_id(0x11), 100)
            .await
            .unwrap();
        let messages: Vec<&str> = records.iter().map(|r| r.record.message.as_str()).collect();
        assert_eq!(messages, ["first", "second"]);

        let proxy_only = store.load_trace(trace(3)).await.unwrap().unwrap();
        assert_eq!(proxy_only.spans().len(), 1);
        let job = store.load_trace(trace(5)).await.unwrap().unwrap();
        let roots: Vec<u8> = job
            .spans()
            .iter()
            .filter(|s| s.depth == 0)
            .map(|s| s.span.span_id.as_ref()[0])
            .collect();
        assert_eq!(roots, [0x51, 0x53], "both roots, by start");

        assert!(
            store.load_trace(trace(4)).await.unwrap().is_none(),
            "an older row has no spans"
        );
        assert!(store.load_trace(trace(99)).await.unwrap().is_none());
        store.close().await;
    }

    #[tokio::test]
    async fn a_trace_is_read_up_to_its_span_cap_with_the_true_count() {
        let (_dir, store) = open().await;
        let over = Trace::MAX_SPANS + 5;
        let spans = (0..over)
            .map(|i| {
                let mut s = span(
                    trace(7),
                    0,
                    None,
                    "notes",
                    "web",
                    i as i64,
                    1,
                    SpanStatus::Unset,
                );
                s.span_id = SpanId::from((i as u64 + 1).to_be_bytes());
                s.parent = (i > 0).then(|| SpanId::from(1u64.to_be_bytes()));
                s
            })
            .collect();
        let mut row = request(7, 0, "notes", "web", 200, 5);
        row.request_id = trace(7);
        store
            .append(&LogBatch {
                spans,
                requests: vec![row],
                ..LogBatch::default()
            })
            .await
            .unwrap();
        let loaded = store.load_trace(trace(7)).await.unwrap().unwrap();
        assert_eq!(loaded.spans().len(), Trace::MAX_SPANS);
        assert_eq!(loaded.total_spans(), over as u64 + 2);
        assert!(loaded.is_truncated());
        assert!(
            loaded.span(span_id(0xa7)).is_some() && loaded.span(span_id(0xc7)).is_some(),
            "the proxy's spans are always kept"
        );
        store.close().await;
    }

    #[tokio::test]
    async fn a_traces_log_records_are_read_with_their_index() {
        let (_dir, store) = open().await;
        let rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(
            "EXPLAIN QUERY PLAN SELECT span_id, count(*) FROM log_records \
             WHERE trace_id = 'x' GROUP BY span_id",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        let plan = rows.into_iter().map(|r| r.3).collect::<Vec<_>>().join("; ");
        assert!(plan.contains("log_records_by_trace"), "{plan}");
        store.close().await;
    }
}
