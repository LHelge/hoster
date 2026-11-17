//! The Traces tab's reads: the trace list, the scatter, one trace and its log records, on
//! seeded request rows and spans.

use std::net::IpAddr;
use std::ops::Range;

use super::*;
use SpanStatus::{Error, Unset};

const MS: i64 = 1_000;

fn at(millis: i64) -> Timestamp {
    Timestamp::from_millis(millis)
}

fn everything() -> Range<Timestamp> {
    at(0)..at(1_000_000_000)
}

fn trace(n: u8) -> TraceId {
    TraceId::from([n; 16])
}

fn sid(n: u8) -> SpanId {
    SpanId::from([n; 8])
}

/// The request row of trace `n`, arrived at `time` ms, with the proxy's span ids `n0` and `n1`
/// (as bytes `0xn0`, `0xn1`) and an upstream start 1 ms after its arrival.
fn request(n: u8, time: i64, stack: &str, path: &str, status: u16, total_ms: u64) -> RequestRow {
    RequestRow {
        time: at(time),
        host: format!("{stack}.example.com"),
        prefix: "/".into(),
        stack: stack.into(),
        service: "web".into(),
        method: "GET".into(),
        path: path.into(),
        status,
        to_header: Some(Duration::from_millis(1)),
        total: Duration::from_millis(total_ms),
        bytes: 10,
        client: IpAddr::from([203, 0, 113, 7]),
        upstream_failed: false,
        request_id: trace(n),
        server_span: Some(sid(n << 4)),
        client_span: Some(sid((n << 4) | 1)),
        upstream_start: Some(MicroTimestamp::from_micros((time + 1) * MS)),
    }
}

/// A stored span of trace `n`, started at `start_us`.
#[allow(
    clippy::too_many_arguments,
    reason = "a test fixture with every field that varies"
)]
fn span(
    n: u8,
    id: u8,
    parent: Option<u8>,
    stack: &str,
    service: &str,
    name: &str,
    start_us: i64,
    duration_ms: u64,
    status: SpanStatus,
) -> Span {
    Span {
        trace_id: trace(n),
        span_id: sid(id),
        parent: parent.map(sid),
        stack: stack.into(),
        service: service.into(),
        name: name.into(),
        kind: SpanKind::Internal,
        start: MicroTimestamp::from_micros(start_us),
        duration: Duration::from_millis(duration_ms),
        status,
        status_message: None,
        attributes: None,
        resource: None,
        events: None,
        received: at(900_000),
    }
}

fn record(
    trace_id: Option<TraceId>,
    span_id: Option<SpanId>,
    time: i64,
    message: &str,
) -> LogRecord {
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
        trace_id,
        span_id,
        received: Some(at(time)),
    }
}

async fn seed(batch: LogBatch) -> (TempDir, LogStore) {
    let (dir, store) = open().await;
    store.append(&batch).await.unwrap();
    (dir, store)
}

/// - trace 1: a request to `notes` at 1 s (200, 50 ms) whose app exported a server span and
///   a child of another service that failed;
/// - trace 2: a request to `notes` at 2 s (200, 300 ms) whose app exports nothing;
/// - trace 3: a request to `blog` at 3 s that hoster answered itself (503, 10 ms);
/// - a request at 4 s from before span ids were stored: no trace;
/// - trace 5: a job `notes`' worker started itself at 5 s (2 s), with a child;
/// - trace 6: a job at 6 s (100 ms) with a second root at 6.5 s: spans were lost.
async fn mixed() -> (TempDir, LogStore) {
    let mut answered = request(3, 3_000, "blog", "/c", 503, 10);
    answered.client_span = None;
    answered.upstream_start = None;
    let mut old = request(4, 4_000, "notes", "/old", 200, 5);
    old.server_span = None;
    old.client_span = None;
    old.upstream_start = None;
    seed(LogBatch {
        requests: vec![
            request(1, 1_000, "notes", "/a", 200, 50),
            request(2, 2_000, "notes", "/b", 200, 300),
            answered,
            old,
        ],
        spans: vec![
            span(
                1,
                0x12,
                Some(0x11),
                "notes",
                "web",
                "GET /a",
                1_002 * MS,
                45,
                Unset,
            ),
            span(
                1,
                0x13,
                Some(0x12),
                "notes",
                "api",
                "lookup",
                1_010 * MS,
                20,
                Error,
            ),
            span(
                5,
                0x50,
                None,
                "notes",
                "worker",
                "nightly",
                5_000 * MS,
                2_000,
                Unset,
            ),
            span(
                5,
                0x51,
                Some(0x50),
                "notes",
                "worker",
                "sweep",
                5_100 * MS,
                500,
                Unset,
            ),
            span(
                6,
                0x60,
                None,
                "notes",
                "worker",
                "hourly",
                6_000 * MS,
                100,
                Unset,
            ),
            span(
                6,
                0x61,
                None,
                "notes",
                "worker",
                "hourly",
                6_500 * MS,
                100,
                Unset,
            ),
        ],
        ..LogBatch::default()
    })
    .await
}

async fn listed(store: &LogStore, query: &TraceQuery) -> Vec<u8> {
    let list = store.traces(query).await.unwrap();
    list.traces.iter().map(|t| t.trace_id.as_ref()[0]).collect()
}

#[tokio::test]
async fn the_list_has_one_row_per_trace_from_its_root_newest_first() {
    let (_dir, store) = mixed().await;
    let list = store
        .traces(&TraceQuery::new(everything(), LogScope::All))
        .await
        .unwrap();
    let ids: Vec<u8> = list.traces.iter().map(|t| t.trace_id.as_ref()[0]).collect();
    assert_eq!(ids, [6, 5, 3, 2, 1]);
    assert_eq!((list.matching, list.total), (5, 5));

    let row = |n: u8| list.traces.iter().find(|t| t.trace_id == trace(n)).unwrap();
    assert_eq!(
        *row(1),
        TraceSummary {
            trace_id: trace(1),
            start: MicroTimestamp::from_micros(1_000 * MS),
            name: "GET /a".into(),
            stack: "notes".into(),
            services: vec![Span::EDGE_SERVICE.into(), "web".into(), "api".into()],
            spans: 4,
            duration: Duration::from_millis(50),
            // The request answered 200, but a span of its app failed.
            status: TraceStatus::Error,
            proxy_only: false,
        }
    );
    // The app sent nothing: only the proxy's two spans, one service.
    let uninstrumented = row(2);
    assert!(uninstrumented.proxy_only);
    assert_eq!(uninstrumented.services, [Span::EDGE_SERVICE]);
    assert_eq!(
        (uninstrumented.spans, uninstrumented.status),
        (2, TraceStatus::Ok)
    );
    // Answered by hoster itself: a server span only, in error for its 503.
    assert_eq!((row(3).spans, row(3).status), (1, TraceStatus::Error));
    assert!(row(3).proxy_only);
    // A job an app started itself is listed by its stored root.
    let job = row(5);
    assert_eq!(
        (job.name.as_str(), job.stack.as_str()),
        ("nightly", "notes")
    );
    assert_eq!(job.services, ["worker"]);
    assert_eq!((job.spans, job.duration), (2, Duration::from_secs(2)));
    assert!(!job.proxy_only);
    // Two roots, one row: the earlier.
    assert_eq!(row(6).start, MicroTimestamp::from_micros(6_000 * MS));
    assert_eq!(row(6).spans, 2);
    store.close().await;
}

#[tokio::test]
async fn the_list_sorts_by_duration_or_errors_first() {
    let (_dir, store) = mixed().await;
    let mut query = TraceQuery::new(everything(), LogScope::All);
    query.sort = TraceSort::Slowest;
    assert_eq!(listed(&store, &query).await, [5, 2, 6, 1, 3]);
    query.sort = TraceSort::ErrorsFirst;
    assert_eq!(listed(&store, &query).await, [3, 1, 6, 5, 2]);
    store.close().await;
}

#[tokio::test]
async fn each_filter_narrows_and_the_total_stays_the_ranges() {
    let (_dir, store) = mixed().await;
    let all = || TraceQuery::new(everything(), LogScope::All);

    let mut query = all();
    query.status = Some(TraceStatus::Error);
    assert_eq!(listed(&store, &query).await, [3, 1]);
    let list = store.traces(&query).await.unwrap();
    assert_eq!((list.matching, list.total), (2, 5));
    query.status = Some(TraceStatus::Ok);
    assert_eq!(listed(&store, &query).await, [6, 5, 2]);

    let mut query = all();
    query.min_duration = Some(Duration::from_millis(100));
    assert_eq!(listed(&store, &query).await, [6, 5, 2]);

    let mut query = all();
    query.status = Some(TraceStatus::Ok);
    query.min_duration = Some(Duration::from_secs(1));
    assert_eq!(listed(&store, &query).await, [5]);

    // Grep on span names: the root's (the proxy's `GET /b`) or a stored span's, ignoring case.
    let mut query = all();
    query.name = Some("LOOK".into());
    assert_eq!(listed(&store, &query).await, [1]);
    query.name = Some("/b".into());
    assert_eq!(listed(&store, &query).await, [2]);
    query.name = Some("hourly".into());
    assert_eq!(listed(&store, &query).await, [6]);
    query.name = Some("%".into());
    assert_eq!(listed(&store, &query).await, [] as [u8; 0]);
    query.name = Some(String::new());
    assert_eq!(listed(&store, &query).await, [6, 5, 3, 2, 1]);

    let blog = TraceQuery::new(everything(), LogScope::Stack("blog".into()));
    assert_eq!(listed(&store, &blog).await, [3]);
    assert_eq!(store.traces(&blog).await.unwrap().total, 1);
    // The count alone, as the Logs tab reads it.
    assert_eq!(
        store
            .count_traces(everything(), LogScope::All)
            .await
            .unwrap(),
        5
    );
    assert_eq!(
        store
            .count_traces(at(2_000)..at(5_000), LogScope::Stack("blog".into()))
            .await
            .unwrap(),
        1
    );
    let hoster = TraceQuery::new(everything(), LogScope::Hoster);
    assert_eq!(store.traces(&hoster).await.unwrap(), TraceList::default());

    // By the root's start, in milliseconds, the end exclusive.
    let range = TraceQuery::new(at(2_000)..at(5_000), LogScope::All);
    assert_eq!(listed(&store, &range).await, [3, 2]);
    assert_eq!(store.traces(&range).await.unwrap().total, 2);
    store.close().await;
}

#[tokio::test]
async fn the_list_pages() {
    let (_dir, store) = mixed().await;
    let mut query = TraceQuery::new(everything(), LogScope::All);
    query.limit = 2;
    assert_eq!(listed(&store, &query).await, [6, 5]);
    query.page = 1;
    assert_eq!(listed(&store, &query).await, [3, 2]);
    query.page = 2;
    assert_eq!(listed(&store, &query).await, [1]);
    query.page = 3;
    let list = store.traces(&query).await.unwrap();
    assert!(list.traces.is_empty());
    assert_eq!((list.matching, list.total), (5, 5));
    store.close().await;
}

#[tokio::test]
async fn scatter_points_are_every_match_by_start() {
    let (_dir, store) = mixed().await;
    let mut query = TraceQuery::new(everything(), LogScope::All);
    query.sort = TraceSort::Slowest;
    query.page = 3;
    let points = store.trace_points(&query).await.unwrap();
    assert_eq!(points.matching, 5);
    let ids: Vec<u8> = points
        .points
        .iter()
        .map(|p| p.trace_id.as_ref()[0])
        .collect();
    // Neither the sort nor the page apply.
    assert_eq!(ids, [1, 2, 3, 5, 6]);
    let slowest = points.points.iter().map(|p| p.duration).max().unwrap();
    assert_eq!(points.slowest, slowest);
    assert_eq!(
        points.points[0],
        TracePoint {
            trace_id: trace(1),
            start: MicroTimestamp::from_micros(1_000 * MS),
            duration: Duration::from_millis(50),
            status: TraceStatus::Error,
        }
    );

    query.status = Some(TraceStatus::Error);
    let errors = store.trace_points(&query).await.unwrap();
    assert_eq!((errors.points.len(), errors.matching), (2, 2));
    query.scope = LogScope::Hoster;
    assert_eq!(
        store.trace_points(&query).await.unwrap(),
        TracePoints::default()
    );
    store.close().await;
}

#[tokio::test]
async fn over_the_cap_scatter_points_are_sampled_evenly() {
    let n = 2_500;
    let requests = (0..n)
        .map(|i: i64| {
            let mut row = request(0, 1_000 + i, "notes", "/", 200, 5);
            let mut id = [0; 16];
            id[8..].copy_from_slice(&(i + 1).to_be_bytes());
            row.request_id = TraceId::from(id);
            if i == 1 {
                row.total = Duration::from_secs(30);
            }
            row
        })
        .collect();
    let mut slow = [0; 16];
    slow[8..].copy_from_slice(&2i64.to_be_bytes());
    let slow = TraceId::from(slow);
    let (_dir, store) = seed(LogBatch {
        requests,
        ..LogBatch::default()
    })
    .await;
    let points = store
        .trace_points(&TraceQuery::new(everything(), LogScope::All))
        .await
        .unwrap();
    assert_eq!(points.matching, 2_500);
    assert_eq!(points.points.len(), TracePoints::CAP as usize);
    // The slowest root of every match, though sampling left it out: trace 2 (start 1,001).
    assert!(!points.points.iter().any(|p| p.trace_id == slow));
    assert_eq!(points.slowest, Duration::from_secs(30));
    let starts: Vec<i64> = points
        .points
        .iter()
        .map(|p| p.start.as_micros() / MS - 1_000)
        .collect();
    // The first of every 2.5 traces: 0, 3, 5, 8, 10, …
    assert_eq!(starts[..5], [0, 3, 5, 8, 10]);
    assert!(starts.windows(2).all(|w| (2..=3).contains(&(w[1] - w[0]))));
    assert_eq!(*starts.last().unwrap(), 2_498);
    store.close().await;
}

#[tokio::test]
async fn a_proxied_trace_reads_with_the_proxys_spans_from_its_row() {
    let (_dir, store) = mixed().await;
    let trace1 = store.load_trace(trace(1)).await.unwrap().unwrap();
    let names: Vec<&str> = trace1
        .spans()
        .iter()
        .map(|s| s.span().name.as_str())
        .collect();
    assert_eq!(names, ["GET /a", "upstream web", "GET /a", "lookup"]);
    let depths: Vec<usize> = trace1.spans().iter().map(TraceSpan::depth).collect();
    assert_eq!(depths, [0, 1, 2, 3]);
    assert!(trace1.spans().iter().all(|s| !s.parent_missing()));
    assert_eq!(trace1.spans()[0].span().service, Span::EDGE_SERVICE);
    assert_eq!(trace1.total_spans(), 4);
    assert!(!trace1.is_truncated());

    // Only the proxy's.
    let trace2 = store.load_trace(trace(2)).await.unwrap().unwrap();
    assert_eq!(trace2.spans().len(), 2);
    // A job's, stored only.
    let trace5 = store.load_trace(trace(5)).await.unwrap().unwrap();
    assert_eq!(trace5.root().unwrap().span().name, "nightly");
    assert_eq!(trace5.spans().len(), 2);
    // Two roots, both kept.
    let trace6 = store.load_trace(trace(6)).await.unwrap().unwrap();
    assert_eq!(trace6.depth(), 1);
    assert_eq!(trace6.spans().len(), 2);

    // A row from before span ids gives nothing, nor does an unknown id.
    assert!(store.load_trace(trace(4)).await.unwrap().is_none());
    assert!(store.load_trace(trace(9)).await.unwrap().is_none());
    store.close().await;
}

#[tokio::test]
async fn app_spans_whose_request_row_is_gone_read_as_roots_with_missing_parents() {
    let (_dir, store) = seed(LogBatch {
        spans: vec![
            span(
                1,
                0x12,
                Some(0x11),
                "notes",
                "web",
                "GET /a",
                1_002 * MS,
                45,
                Unset,
            ),
            span(
                1,
                0x13,
                Some(0x12),
                "notes",
                "api",
                "lookup",
                1_010 * MS,
                20,
                Unset,
            ),
        ],
        ..LogBatch::default()
    })
    .await;
    let read = store.load_trace(trace(1)).await.unwrap().unwrap();
    assert!(read.root().unwrap().parent_missing());
    assert_eq!(read.depth(), 2);
    // Without a root it is not listed.
    let list = store
        .traces(&TraceQuery::new(everything(), LogScope::All))
        .await
        .unwrap();
    assert_eq!(list.total, 0);
    store.close().await;
}

#[tokio::test]
async fn a_large_trace_reads_up_to_the_cap_with_its_true_count() {
    let extra = 5;
    let stored = Trace::SPAN_CAP + extra;
    let spans = (0..stored)
        .map(|i| {
            let mut child = span(
                1,
                0,
                Some(0x11),
                "notes",
                "web",
                "step",
                1_002 * MS,
                1,
                Unset,
            );
            let mut id = [0xee; 8];
            id[4..].copy_from_slice(&u32::try_from(i).unwrap().to_be_bytes());
            child.span_id = SpanId::from(id);
            child.start = MicroTimestamp::from_micros(1_002 * MS + i64::try_from(i).unwrap());
            child
        })
        .collect();
    let (_dir, store) = seed(LogBatch {
        requests: vec![request(1, 1_000, "notes", "/a", 200, 50)],
        spans,
        ..LogBatch::default()
    })
    .await;
    let read = store.load_trace(trace(1)).await.unwrap().unwrap();
    assert_eq!(read.spans().len(), Trace::SPAN_CAP);
    assert_eq!(read.total_spans(), u64::try_from(stored + 2).unwrap());
    assert!(read.is_truncated());
    // The proxy's spans are always among them, and the earliest stored ones.
    assert_eq!(read.root().unwrap().span().service, Span::EDGE_SERVICE);
    let last = read.spans().last().unwrap().span();
    assert_eq!(
        last.start,
        MicroTimestamp::from_micros(1_002 * MS + i64::try_from(Trace::SPAN_CAP - 3).unwrap())
    );
    store.close().await;
}

#[tokio::test]
async fn a_traces_records_are_counted_and_a_spans_listed() {
    let app_span = sid(0x12);
    let (_dir, store) = seed(LogBatch {
        records: vec![
            record(Some(trace(1)), Some(app_span), 1_005, "second"),
            record(Some(trace(1)), Some(app_span), 1_003, "first"),
            record(Some(trace(1)), Some(sid(0x10)), 1_040, "upstream answered"),
            record(Some(trace(1)), None, 1_041, "no span"),
            record(Some(trace(5)), Some(sid(0x50)), 5_000, "job"),
            record(None, None, 1_004, "outside any trace"),
        ],
        ..LogBatch::default()
    })
    .await;

    // Every record of the trace, those without a span included.
    assert_eq!(store.trace_record_count(trace(1)).await.unwrap(), 4);
    assert_eq!(store.trace_record_count(trace(5)).await.unwrap(), 1);
    assert_eq!(store.trace_record_count(trace(2)).await.unwrap(), 0);

    let records = store.span_records(trace(1), app_span, 10).await.unwrap();
    let messages: Vec<&str> = records.iter().map(|r| r.record.message.as_str()).collect();
    assert_eq!(messages, ["first", "second"]);
    assert_eq!(
        store
            .span_records(trace(1), app_span, 1)
            .await
            .unwrap()
            .len(),
        1
    );
    store.close().await;
}

#[tokio::test]
async fn a_traces_records_are_read_with_the_trace_index() {
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
    let count = plan("SELECT count(*) FROM log_records WHERE trace_id = 'x'").await;
    assert!(count.contains("log_records_by_trace"), "{count}");
    let span =
        plan("SELECT * FROM log_records WHERE trace_id = 'x' AND span_id = 'y' ORDER BY time, id")
            .await;
    assert!(span.contains("log_records_by_trace"), "{span}");
    store.close().await;
}

#[tokio::test]
async fn the_list_reads_its_roots_and_spans_with_their_indexes() {
    let (_dir, store) = open().await;
    let mut query = TraceQuery::new(everything(), LogScope::All);
    query.status = Some(TraceStatus::Error);
    let plan = |query: TraceQuery| {
        let store = store.clone();
        async move {
            let rows: Vec<(i64, i64, i64, String)> = query
                .page_sql("EXPLAIN QUERY PLAN ")
                .build_query_as()
                .fetch_all(&store.pool)
                .await
                .unwrap();
            rows.into_iter().map(|r| r.3).collect::<Vec<_>>().join("; ")
        }
    };
    let all = plan(query.clone()).await;
    for index in ["requests_by_time", "spans_roots_by_start", "spans_by_trace"] {
        assert!(all.contains(index), "{index}: {all}");
    }
    query.scope = LogScope::Stack("notes".into());
    let stack = plan(query).await;
    for index in ["requests_by_stack", "spans_roots_by_stack"] {
        assert!(stack.contains(index), "{index}: {stack}");
    }
    store.close().await;
}

/// Unfiltered and newest first, a page reads its rows' span figures for those rows alone; a
/// status filter or errors first reads them for every root. Both give the same rows.
#[tokio::test]
async fn the_page_only_path_and_the_every_root_path_agree() {
    let (_dir, store) = mixed().await;
    let read = |query: TraceQuery| {
        let store = store.clone();
        async move { store.traces(&query).await.unwrap() }
    };
    let newest = read(TraceQuery::new(everything(), LogScope::All)).await;
    assert_eq!(newest.traces.len(), 5);

    for status in TraceStatus::ALL {
        let mut query = TraceQuery::new(everything(), LogScope::All);
        query.status = Some(status);
        let filtered = read(query).await;
        let expected: Vec<TraceSummary> = newest
            .traces
            .iter()
            .filter(|t| t.status == status)
            .cloned()
            .collect();
        assert_eq!(filtered.traces, expected, "{status}");
        assert_eq!(filtered.matching, u64::try_from(expected.len()).unwrap());
        assert_eq!(filtered.total, newest.total);
    }

    let mut query = TraceQuery::new(everything(), LogScope::All);
    query.sort = TraceSort::ErrorsFirst;
    let errors_first = read(query).await;
    let mut expected = newest.traces.clone();
    // Stable: newest first within each status.
    expected.sort_by_key(|t| t.status != TraceStatus::Error);
    assert_eq!(errors_first.traces, expected);

    // The scatter reads its points' status after sampling, or before with a status filter.
    let all = store
        .trace_points(&TraceQuery::new(everything(), LogScope::All))
        .await
        .unwrap();
    for point in &all.points {
        let listed = newest.traces.iter().find(|t| t.trace_id == point.trace_id);
        assert_eq!(point.status, listed.unwrap().status);
    }
    let mut query = TraceQuery::new(everything(), LogScope::All);
    query.status = Some(TraceStatus::Error);
    let errors = store.trace_points(&query).await.unwrap();
    let expected: Vec<TracePoint> = all
        .points
        .iter()
        .filter(|p| p.status == TraceStatus::Error)
        .copied()
        .collect();
    assert_eq!(errors.points, expected);
    store.close().await;
}
