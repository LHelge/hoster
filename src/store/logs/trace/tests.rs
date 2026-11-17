use std::collections::HashSet;
use std::net::IpAddr;

use super::*;
use crate::store::Timestamp;

/// The trace of the sample `t1` in `docs/design/Telemetry.dc.html` (`renderVals()`) starts here.
const BASE_US: i64 = 1_800_000_000_000_000;

fn trace_id() -> TraceId {
    "4bf92f3577b34da6a3ce929d0e0e4736".parse().unwrap()
}

/// The span `s<n>` of a mockup trace: `n` as every byte of its id.
fn id(n: u8) -> SpanId {
    SpanId::from([n; 8])
}

/// A span of `t1`, timed as the mockup times it: ms after the trace's start.
fn span(
    n: u8,
    parent: Option<u8>,
    service: &str,
    kind: SpanKind,
    start_ms: f64,
    duration_ms: f64,
    error: bool,
) -> Span {
    let us = |ms: f64| (ms * 1000.0).round() as i64;
    Span {
        trace_id: trace_id(),
        span_id: id(n),
        parent: parent.map(id),
        stack: "outline".into(),
        service: service.into(),
        name: format!("s{n}"),
        kind,
        start: MicroTimestamp::from_micros(BASE_US + us(start_ms)),
        duration: Duration::from_micros(us(duration_ms) as u64),
        status: if error {
            SpanStatus::Error
        } else {
            SpanStatus::Unset
        },
        status_message: None,
        attributes: None,
        resource: None,
        events: None,
        received: Timestamp::from_millis(BASE_US / 1000),
    }
}

/// The mockup's `t1`: a proxied `POST /api/documents.info` that timed out in Postgres. The
/// proxy's spans are `hoster`'s here, as [`RequestRow::edge_spans`] builds them.
fn t1() -> Vec<Span> {
    use SpanKind::{Client, Internal, Server};
    let app = "outline-app";
    vec![
        span(0, None, Span::EDGE_SERVICE, Server, 0.0, 1912.4, true),
        span(1, Some(0), Span::EDGE_SERVICE, Client, 0.6, 1910.9, true),
        span(2, Some(1), app, Server, 3.8, 1905.2, true),
        span(3, Some(2), app, Internal, 5.1, 17.6, false),
        span(4, Some(3), app, Client, 6.9, 1.8, false),
        span(5, Some(3), app, Client, 10.4, 9.2, false),
        span(6, Some(2), app, Internal, 23.9, 1880.3, true),
        span(12, Some(6), app, Client, 24.6, 0.9, false),
        span(7, Some(6), app, Client, 26.2, 14.1, false),
        span(8, Some(6), app, Client, 41.0, 8.3, false),
        span(9, Some(6), app, Client, 51.7, 1800.4, true),
        span(10, Some(6), app, Client, 1853.6, 2.7, false),
    ]
}

fn names<'a>(spans: impl IntoIterator<Item = &'a TraceSpan>) -> Vec<String> {
    spans.into_iter().map(|s| s.span().name.clone()).collect()
}

fn ms(ms: f64) -> Duration {
    Duration::from_micros((ms * 1000.0).round() as u64)
}

#[test]
fn rows_are_depth_first_with_children_by_start_and_their_depth() {
    let mut spans = t1();
    spans.reverse();
    let trace = Trace::new(trace_id(), spans, 12);
    assert_eq!(
        names(trace.spans()),
        [
            "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s12", "s7", "s8", "s9", "s10"
        ]
    );
    let depths: Vec<usize> = trace.spans().iter().map(TraceSpan::depth).collect();
    assert_eq!(depths, [0, 1, 2, 3, 4, 4, 3, 4, 4, 4, 4, 4]);
    assert_eq!(trace.depth(), 5);
    assert!(trace.spans().iter().all(|s| !s.parent_missing()));
    assert!(!trace.is_truncated());
    assert_eq!(trace.total_spans(), 12);
    assert_eq!(trace.root().unwrap().span().name, "s0");
    let s6 = trace.span(id(6)).unwrap();
    assert_eq!(trace.parent_of(s6).unwrap().span().name, "s2");
    assert_eq!(
        names(trace.children_of(s6)),
        ["s12", "s7", "s8", "s9", "s10"]
    );
}

#[test]
fn the_header_figures_of_t1() {
    let trace = Trace::new(trace_id(), t1(), 12);
    assert_eq!(trace.start(), Some(MicroTimestamp::from_micros(BASE_US)));
    assert_eq!(trace.duration(), ms(1912.4));
    assert_eq!(trace.errors(), 5);
    assert!(trace.is_error());
    assert_eq!(
        trace.services(),
        [(Span::EDGE_SERVICE, 2), ("outline-app", 10)]
    );
    let s9 = trace.span(id(9)).unwrap();
    assert_eq!(trace.offset(s9), ms(51.7));
}

#[test]
fn the_critical_path_follows_the_child_that_ended_last() {
    let trace = Trace::new(trace_id(), t1(), 12);
    // s6 ends at 1904.2 ms, after s3; under s6, s10 ends at 1856.3 ms, after s9's 1852.1.
    assert_eq!(
        names(trace.critical_path()),
        ["s0", "s1", "s2", "s6", "s10"]
    );
    assert!(trace.span(id(10)).unwrap().on_critical_path());
    assert!(!trace.span(id(9)).unwrap().on_critical_path());
}

#[test]
fn self_time_is_the_duration_less_the_union_of_the_children() {
    let trace = Trace::new(trace_id(), t1(), 12);
    let own = |n| trace.span(id(n)).unwrap().self_time();
    assert_eq!(own(0), ms(1.5));
    assert_eq!(own(1), ms(5.7));
    assert_eq!(own(2), ms(7.3));
    assert_eq!(own(3), ms(6.6));
    assert_eq!(own(6), ms(53.9));
    // A leaf spends all of its time itself.
    assert_eq!(own(9), ms(1800.4));
}

#[test]
fn overlapping_and_overhanging_children_count_once_and_inside_the_parent() {
    use SpanKind::Internal;
    let trace = Trace::new(
        trace_id(),
        vec![
            span(1, None, "api", Internal, 0.0, 100.0, false),
            // 10..40 and 30..60 overlap: 50 ms covered.
            span(2, Some(1), "api", Internal, 10.0, 30.0, false),
            span(3, Some(1), "api", Internal, 30.0, 30.0, false),
            // Runs past the parent's end at 100: only 90..100 counts.
            span(4, Some(1), "api", Internal, 90.0, 50.0, false),
        ],
        4,
    );
    assert_eq!(trace.span(id(1)).unwrap().self_time(), ms(40.0));
    assert_eq!(trace.duration(), ms(140.0));
}

#[test]
fn a_trace_opens_on_the_end_of_its_first_error_chain_or_its_root() {
    let trace = Trace::new(trace_id(), t1(), 12);
    assert_eq!(trace.default_span().unwrap().span().name, "s9");

    let ok: Vec<Span> = t1()
        .into_iter()
        .map(|mut s| {
            s.status = SpanStatus::Ok;
            s
        })
        .collect();
    let trace = Trace::new(trace_id(), ok, 12);
    assert_eq!(trace.default_span().unwrap().span().name, "s0");
    assert!(!trace.is_error());
}

#[test]
fn collapsing_hides_the_rows_under_a_span_and_to_services_collapses_entry_spans() {
    let trace = Trace::new(trace_id(), t1(), 12);
    let entries = trace.service_entries();
    // s0 is the root; s2 is outline's server span with rows under it.
    assert_eq!(entries, HashSet::from([id(2)]));
    assert_eq!(names(trace.visible(&entries)), ["s0", "s1", "s2"]);
    assert_eq!(trace.span(id(2)).unwrap().descendants(), 9);
    assert_eq!(trace.span(id(0)).unwrap().descendants(), 11);

    let collapsed = HashSet::from([id(3), id(9)]);
    assert_eq!(
        names(trace.visible(&collapsed)),
        ["s0", "s1", "s2", "s3", "s6", "s12", "s7", "s8", "s9", "s10"]
    );
    assert_eq!(trace.visible(&HashSet::new()).len(), 12);
}

#[test]
fn a_span_with_a_missing_parent_is_shown_under_the_root_and_marked() {
    use SpanKind::Internal;
    let trace = Trace::new(
        trace_id(),
        vec![
            span(1, None, "api", Internal, 0.0, 100.0, false),
            span(2, Some(1), "api", Internal, 30.0, 10.0, false),
            // Its parent 9 was lost.
            span(3, Some(9), "api", Internal, 20.0, 10.0, false),
            span(4, Some(3), "api", Internal, 21.0, 5.0, false),
        ],
        4,
    );
    assert_eq!(names(trace.spans()), ["s1", "s3", "s4", "s2"]);
    let missing: Vec<bool> = trace
        .spans()
        .iter()
        .map(TraceSpan::parent_missing)
        .collect();
    assert_eq!(missing, [false, true, false, false]);
    let depths: Vec<usize> = trace.spans().iter().map(TraceSpan::depth).collect();
    assert_eq!(depths, [0, 1, 2, 1]);
}

#[test]
fn without_a_root_every_span_with_a_missing_parent_is_a_root_by_start() {
    use SpanKind::{Internal, Server};
    // The proxy's spans are gone: outline's server spans of two attempts are left.
    let trace = Trace::new(
        trace_id(),
        vec![
            span(5, Some(99), "api", Server, 50.0, 10.0, false),
            span(2, Some(98), "api", Server, 10.0, 10.0, false),
            span(3, Some(2), "api", Internal, 12.0, 1.0, false),
        ],
        3,
    );
    assert_eq!(names(trace.spans()), ["s2", "s3", "s5"]);
    assert!(trace.spans()[0].parent_missing() && trace.spans()[2].parent_missing());
    assert_eq!(names(trace.critical_path()), ["s2", "s3", "s5"]);
    assert_eq!(trace.depth(), 2);
}

#[test]
fn several_roots_are_kept_by_start() {
    use SpanKind::Internal;
    let trace = Trace::new(
        trace_id(),
        vec![
            span(2, None, "worker", Internal, 40.0, 10.0, false),
            span(1, None, "worker", Internal, 0.0, 10.0, false),
            span(3, Some(2), "worker", Internal, 41.0, 1.0, false),
        ],
        3,
    );
    assert_eq!(names(trace.spans()), ["s1", "s2", "s3"]);
    assert!(trace.spans().iter().all(|s| !s.parent_missing()));
}

#[test]
fn cycles_and_self_parents_are_cut_as_missing_parents() {
    use SpanKind::Internal;
    let trace = Trace::new(
        trace_id(),
        vec![
            span(1, None, "api", Internal, 0.0, 100.0, false),
            span(2, Some(3), "api", Internal, 10.0, 5.0, false),
            span(3, Some(2), "api", Internal, 12.0, 5.0, false),
            span(4, Some(4), "api", Internal, 20.0, 5.0, false),
        ],
        4,
    );
    assert_eq!(names(trace.spans()), ["s1", "s2", "s3", "s4"]);
    let depths: Vec<usize> = trace.spans().iter().map(TraceSpan::depth).collect();
    assert_eq!(depths, [0, 1, 2, 1]);
    let missing: Vec<bool> = trace
        .spans()
        .iter()
        .map(TraceSpan::parent_missing)
        .collect();
    assert_eq!(missing, [false, true, false, true]);
}

#[test]
fn a_cut_read_keeps_the_true_count() {
    let mut spans = t1();
    spans.truncate(5);
    let trace = Trace::new(trace_id(), spans, 12);
    assert!(trace.is_truncated());
    assert_eq!(trace.total_spans(), 12);
    assert_eq!(trace.spans().len(), 5);
    // Never fewer than read.
    assert_eq!(Trace::new(trace_id(), t1(), 3).total_spans(), 12);
}

#[test]
fn an_empty_trace_has_no_rows_or_figures() {
    let trace = Trace::new(trace_id(), Vec::new(), 0);
    assert!(trace.is_empty());
    assert!(trace.root().is_none() && trace.default_span().is_none());
    assert_eq!((trace.depth(), trace.duration()), (0, Duration::ZERO));
}

fn request() -> RequestRow {
    RequestRow {
        time: Timestamp::from_millis(1_800_000_000_000),
        host: "wiki.example.net".into(),
        prefix: "/".into(),
        stack: "outline".into(),
        service: "app".into(),
        method: "POST".into(),
        path: "/api/documents.info".into(),
        status: 500,
        to_header: Some(Duration::from_millis(1911)),
        total: Duration::from_millis(1912),
        bytes: 120,
        client: IpAddr::from([192, 168, 1, 34]),
        upstream_failed: false,
        request_id: trace_id(),
        server_span: Some(id(0xa0)),
        client_span: Some(id(0xb0)),
        upstream_start: Some(MicroTimestamp::from_micros(1_800_000_000_000_600)),
    }
}

#[test]
fn a_request_row_gives_the_proxys_server_and_client_spans() {
    let spans = request().edge_spans();
    assert_eq!(spans.len(), 2);
    let (server, client) = (&spans[0], &spans[1]);

    assert_eq!(server.trace_id, trace_id());
    assert_eq!((server.span_id, server.parent), (id(0xa0), None));
    assert_eq!(server.kind, SpanKind::Server);
    assert_eq!(server.service, Span::EDGE_SERVICE);
    assert_eq!(server.stack, "outline");
    assert_eq!(server.name, "POST /api/documents.info");
    assert_eq!(
        server.start,
        MicroTimestamp::from_micros(1_800_000_000_000_000)
    );
    assert_eq!(server.duration, Duration::from_millis(1912));
    assert_eq!(server.status, SpanStatus::Error);
    assert_eq!(server.status_message.as_deref(), Some("responded 500"));
    let attributes: serde_json::Value =
        serde_json::from_str(server.attributes.as_deref().unwrap()).unwrap();
    assert_eq!(attributes["http.response.status_code"], 500);
    assert_eq!(attributes["server.address"], "wiki.example.net");
    assert_eq!(attributes["client.address"], "192.168.1.34");

    assert_eq!((client.span_id, client.parent), (id(0xb0), Some(id(0xa0))));
    assert_eq!(client.kind, SpanKind::Client);
    assert_eq!(client.name, "upstream app");
    assert_eq!(client.start, request().upstream_start.unwrap());
    // It ends with the request.
    assert_eq!(client.end(), server.end());
    assert_eq!(client.duration, Duration::from_micros(1_911_400));
}

#[test]
fn edge_span_status_follows_the_upstream_and_the_answer() {
    let mut ok = request();
    ok.status = 404;
    assert!(
        ok.edge_spans()
            .iter()
            .all(|s| s.status == SpanStatus::Unset)
    );

    let mut failed = request();
    failed.status = 502;
    failed.upstream_failed = true;
    for span in failed.edge_spans() {
        assert_eq!(span.status, SpanStatus::Error);
        assert_eq!(
            span.status_message.as_deref(),
            Some("upstream request failed")
        );
    }

    // A client that left before the answer: no status code, no error.
    let mut left = request();
    left.status = 0;
    let spans = left.edge_spans();
    assert_eq!(spans[0].status, SpanStatus::Unset);
    let attributes: serde_json::Value =
        serde_json::from_str(spans[0].attributes.as_deref().unwrap()).unwrap();
    assert!(attributes.get("http.response.status_code").is_none());
}

#[test]
fn a_request_without_an_upstream_or_span_ids_gives_fewer_spans() {
    let mut answered = request();
    answered.client_span = None;
    answered.upstream_start = None;
    let spans = answered.edge_spans();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].kind, SpanKind::Server);

    // A row from before the span ids were stored.
    let mut old = request();
    old.server_span = None;
    old.client_span = None;
    old.upstream_start = None;
    assert!(old.edge_spans().is_empty());
}

#[test]
fn edge_spans_and_app_spans_make_one_tree() {
    let mut spans = request().edge_spans();
    let mut app = span(2, Some(0xb0), "app", SpanKind::Server, 3.8, 1900.0, true);
    app.span_id = id(2);
    spans.push(app);
    let trace = Trace::new(trace_id(), spans, 3);
    assert_eq!(
        names(trace.spans()),
        ["POST /api/documents.info", "upstream app", "s2"]
    );
    assert!(trace.spans().iter().all(|s| !s.parent_missing()));
    assert_eq!(trace.depth(), 3);
}
