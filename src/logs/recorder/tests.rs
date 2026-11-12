use std::net::{IpAddr, Ipv4Addr};

use tempfile::TempDir;

use super::*;
use crate::store::{Level, RequestId, Timestamp};

const SHORT: Duration = Duration::from_millis(20);
const NEVER: Duration = Duration::from_secs(3600);
const WAIT: Duration = Duration::from_secs(5);

/// stack, service, generation, stream, level, fields, cut, marker of a stored record.
type StoredRecord = (
    Option<String>,
    String,
    Option<i64>,
    String,
    Option<String>,
    Option<String>,
    bool,
    bool,
);

async fn store() -> (TempDir, LogStore) {
    let dir = TempDir::new().unwrap();
    let store = LogStore::open(dir.path()).await.unwrap();
    (dir, store)
}

fn record(stream: Stream, message: &str) -> LogRecord {
    LogRecord {
        time: Timestamp::now(),
        stack: (stream != Stream::Hoster).then(|| "notes".to_owned()),
        service: if stream == Stream::Hoster {
            "hoster"
        } else {
            "api"
        }
        .to_owned(),
        generation: (stream != Stream::Hoster).then_some(3),
        stream,
        level: Some(Level::Info),
        message: message.to_owned(),
        fields: Some(r#"{"user":"me"}"#.to_owned()),
        cut: false,
        marker: false,
    }
}

fn request(path: &str) -> RequestRow {
    RequestRow {
        time: Timestamp::now(),
        host: "notes.example.com".to_owned(),
        prefix: "/".to_owned(),
        stack: "notes".to_owned(),
        service: "api".to_owned(),
        method: "GET".to_owned(),
        path: path.to_owned(),
        status: 200,
        to_header: Some(Duration::from_millis(12)),
        total: Duration::from_millis(15),
        bytes: 612,
        client: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        upstream_failed: false,
        request_id: RequestId::from([7; 16]),
    }
}

fn cursor(container: &str, time_ns: i64) -> LogCursor {
    LogCursor {
        container: container.to_owned(),
        time_ns,
    }
}

/// How many records and request rows the store holds.
async fn counts(store: &LogStore) -> (i64, i64) {
    sqlx::query_as("SELECT (SELECT count(*) FROM log_records), (SELECT count(*) FROM requests)")
        .fetch_one(store.pool())
        .await
        .unwrap()
}

/// Waits until the sink's head satisfies `done`.
async fn head_reaches(sink: &LogSink, done: impl FnMut(&LogHead) -> bool) -> LogHead {
    let mut head = sink.head();
    *tokio::time::timeout(WAIT, head.wait_for(done))
        .await
        .expect("the recorder commits")
        .unwrap()
}

#[tokio::test]
async fn offered_items_are_committed_together_with_their_newest_cursors() {
    let (_dir, store) = store().await;
    let (recorder, sink) = LogRecorder::new(store.clone()).await.unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(recorder.with_interval(SHORT).run(stop.clone()));

    assert!(sink.try_record_line(record(Stream::Stdout, "one"), cursor("c1", 10)));
    assert!(sink.try_record_line(record(Stream::Stderr, "two"), cursor("c1", 20)));
    assert!(sink.try_record_line(record(Stream::Stdout, "three"), cursor("c2", 5)));
    assert!(sink.try_record(record(Stream::Hoster, "deploying")));
    assert!(sink.try_request(request("/a")));

    let head = head_reaches(&sink, |head| head.record == 4 && head.request == 1).await;
    assert_eq!(head, store.head().await.unwrap());
    assert_eq!(counts(&store).await, (4, 1));
    assert_eq!(store.cursor("c1").await.unwrap(), Some(20));
    assert_eq!(store.cursor("c2").await.unwrap(), Some(5));
    assert!(sink.drops().is_empty());

    stop.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn records_are_stored_as_offered() {
    let (_dir, store) = store().await;
    let (recorder, sink) = LogRecorder::new(store.clone()).await.unwrap();
    let mut hoster = record(Stream::Hoster, "certificate ordered");
    hoster.level = None;
    hoster.fields = None;
    hoster.marker = true;
    hoster.cut = true;
    sink.try_record(hoster);
    sink.try_request(request("/b"));
    let stop = CancellationToken::new();
    stop.cancel();
    recorder.run(stop).await;

    let row: StoredRecord = sqlx::query_as(
        "SELECT stack, service, generation, stream, level, fields, cut, marker \
             FROM log_records",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            None,
            "hoster".to_owned(),
            None,
            "hoster".to_owned(),
            None,
            None,
            true,
            true
        )
    );
    let request: (String, i64, Option<i64>, i64, String, bool, String) = sqlx::query_as(
        "SELECT path, status, header_ms, total_ms, client, upstream_failed, request_id \
         FROM requests",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(
        request,
        (
            "/b".to_owned(),
            200,
            Some(12),
            15,
            "192.0.2.1".to_owned(),
            false,
            "07".repeat(16)
        )
    );
}

#[tokio::test]
async fn a_full_batch_is_committed_without_waiting_for_the_interval() {
    let (_dir, store) = store().await;
    let (recorder, sink) = LogRecorder::new(store.clone()).await.unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(recorder.with_interval(NEVER).run(stop.clone()));

    let cap = i64::try_from(LogRecorder::BATCH_CAP).unwrap();
    for i in 0..LogRecorder::BATCH_CAP {
        // The channel holds more than a batch, so none of these is dropped.
        assert!(sink.try_record(record(Stream::Hoster, &format!("line {i}"))));
    }
    head_reaches(&sink, |head| head.record == cap).await;
    assert_eq!(counts(&store).await, (cap, 0));

    stop.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn a_full_channel_drops_and_counts_by_kind_without_blocking() {
    let (_dir, store) = store().await;
    // Not running: nothing drains the channel.
    let (_recorder, sink) = LogRecorder::new(store).await.unwrap();
    for _ in 0..LogRecorder::CHANNEL_CAPACITY {
        assert!(sink.try_request(request("/fill")));
    }
    assert!(!sink.try_record_line(record(Stream::Stdout, "x"), cursor("c", 1)));
    assert!(!sink.try_record(record(Stream::Hoster, "y")));
    assert!(!sink.try_record(record(Stream::Hoster, "z")));
    assert!(!sink.try_request(request("/over")));
    assert_eq!(
        sink.drops(),
        Drops {
            records: 1,
            hoster_records: 2,
            requests: 1,
            ..Drops::default()
        }
    );
}

#[tokio::test]
async fn a_disconnected_sink_drops_and_counts_everything() {
    let sink = LogSink::disconnected();
    assert!(!sink.try_record_line(record(Stream::Stdout, "x"), cursor("c", 1)));
    assert!(!sink.try_record(record(Stream::Hoster, "y")));
    assert!(!sink.try_request(request("/")));
    assert_eq!(
        sink.drops(),
        Drops {
            records: 1,
            hoster_records: 1,
            requests: 1,
            ..Drops::default()
        }
    );
    assert_eq!(*sink.head().borrow(), LogHead::default());
}

#[tokio::test]
async fn stopping_commits_what_is_still_queued() {
    let (_dir, store) = store().await;
    let (recorder, sink) = LogRecorder::new(store.clone()).await.unwrap();
    for i in 0..10 {
        sink.try_record_line(record(Stream::Stdout, &format!("{i}")), cursor("c", i));
    }
    let stop = CancellationToken::new();
    stop.cancel();
    recorder.with_interval(NEVER).run(stop).await;
    assert_eq!(counts(&store).await, (10, 0));
    assert_eq!(store.cursor("c").await.unwrap(), Some(9));
    assert_eq!(*sink.head().borrow(), store.head().await.unwrap());
}

#[tokio::test]
async fn the_head_starts_at_what_the_store_holds() {
    let (_dir, store) = store().await;
    let (recorder, sink) = LogRecorder::new(store.clone()).await.unwrap();
    sink.try_request(request("/a"));
    let stop = CancellationToken::new();
    stop.cancel();
    recorder.run(stop).await;

    let (_recorder, sink) = LogRecorder::new(store).await.unwrap();
    assert_eq!(
        *sink.head().borrow(),
        LogHead {
            record: 0,
            request: 1
        }
    );
}

#[tokio::test]
async fn a_failed_commit_loses_its_batch_counted_and_the_recorder_keeps_going() {
    let (_dir, store) = store().await;
    let (recorder, sink) = LogRecorder::new(store.clone()).await.unwrap();
    let stop = CancellationToken::new();
    let task = tokio::spawn(recorder.with_interval(SHORT).run(stop.clone()));
    // Every later commit fails: the pool is closed.
    store.close().await;

    sink.try_record(record(Stream::Hoster, "a"));
    sink.try_request(request("/a"));
    let failed = |sink: &LogSink, n| {
        let sink = sink.clone();
        async move {
            tokio::time::timeout(WAIT, async {
                while sink.drops().failed_commits < n {
                    tokio::time::sleep(SHORT).await;
                }
            })
            .await
            .expect("the commit fails");
        }
    };
    failed(&sink, 1).await;
    sink.try_record(record(Stream::Hoster, "b"));
    failed(&sink, 2).await;
    assert_eq!(
        sink.drops(),
        Drops {
            failed_commits: 2,
            lost_in_failed_commits: 3,
            ..Drops::default()
        }
    );
    stop.cancel();
    task.await.unwrap();
}
