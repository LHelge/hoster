//! The waterfall's layout on the mockup's traces `t1` and `t2` (`docs/design/Telemetry.dc.html`,
//! `renderVals()`), without templates.

use std::time::Duration;

use serde_json::json;

use super::details::{SpanLog, SpanLogs};
use super::*;
use crate::store::{LogEntry, MicroTimestamp, SpanKind, SpanStatus, TraceId};

/// When the mockup's traces start: 2026-11-12 19:58:41.081 UTC.
const BASE_US: i64 = 1_794_513_521_081_000;

fn trace_id() -> TraceId {
    "4bf92f3577b34da6a3ce929d0e0e4736".parse().unwrap()
}

/// The span `n` of a mockup trace: `n` as every byte of its id.
fn id(n: u8) -> SpanId {
    SpanId::from([n; 8])
}

/// A span of a mockup trace, timed as the mockup times it: ms after the trace's start.
struct Mock {
    n: u8,
    parent: Option<u8>,
    service: &'static str,
    name: &'static str,
    at_ms: f64,
    ms: f64,
    error: bool,
    attributes: Option<serde_json::Value>,
}

impl From<Mock> for Span {
    fn from(mock: Mock) -> Self {
        let us = |ms: f64| (ms * 1000.0).round() as i64;
        Span {
            trace_id: trace_id(),
            span_id: id(mock.n),
            parent: mock.parent.map(id),
            stack: "outline".into(),
            service: mock.service.into(),
            name: mock.name.into(),
            kind: SpanKind::Internal,
            start: MicroTimestamp::from_micros(BASE_US + us(mock.at_ms)),
            duration: Duration::from_micros(us(mock.ms) as u64),
            status: if mock.error {
                SpanStatus::Error
            } else {
                SpanStatus::Unset
            },
            status_message: None,
            attributes: mock.attributes.map(|a| a.to_string()),
            resource: None,
            events: None,
            received: Timestamp::from_millis(BASE_US / 1000),
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "a test fixture with every field that varies"
)]
fn mock(
    n: u8,
    parent: Option<u8>,
    service: &'static str,
    name: &'static str,
    at_ms: f64,
    ms: f64,
    error: bool,
    attributes: Option<serde_json::Value>,
) -> Span {
    Span::from(Mock {
        n,
        parent,
        service,
        name,
        at_ms,
        ms,
        error,
        attributes,
    })
}

fn pg() -> Option<serde_json::Value> {
    Some(json!({ "db.system": "postgresql", "db.namespace": "outline" }))
}

fn redis() -> Option<serde_json::Value> {
    Some(json!({ "db.system": "redis" }))
}

fn exec() -> Option<serde_json::Value> {
    Some(json!({ "process.command": "git" }))
}

const PROXY: &str = Span::EDGE_SERVICE;

/// The mockup's `t1`: a proxied `POST /api/documents.info` that timed out in Postgres.
fn t1() -> Vec<Span> {
    let app = "outline-app";
    vec![
        mock(
            0,
            None,
            PROXY,
            "POST /api/documents.info",
            0.0,
            1912.4,
            true,
            None,
        ),
        mock(
            1,
            Some(0),
            PROXY,
            "upstream outline",
            0.6,
            1910.9,
            true,
            None,
        ),
        mock(
            2,
            Some(1),
            app,
            "POST /api/documents.info",
            3.8,
            1905.2,
            true,
            None,
        ),
        mock(3, Some(2), app, "authenticate", 5.1, 17.6, false, None),
        mock(4, Some(3), app, "GET", 6.9, 1.8, false, redis()),
        mock(5, Some(3), app, "SELECT users", 10.4, 9.2, false, pg()),
        mock(6, Some(2), app, "documents.info", 23.9, 1880.3, true, None),
        mock(12, Some(6), app, "GET", 24.6, 0.9, false, redis()),
        mock(7, Some(6), app, "SELECT documents", 26.2, 14.1, false, pg()),
        mock(
            8,
            Some(6),
            app,
            "SELECT collections",
            41.0,
            8.3,
            false,
            pg(),
        ),
        mock(
            9,
            Some(6),
            app,
            "SELECT revisions",
            51.7,
            1800.4,
            true,
            pg(),
        ),
        mock(10, Some(6), app, "ROLLBACK", 1853.6, 2.7, false, pg()),
    ]
}

/// The mockup's `t2`: a gitea page that shells out to git.
fn t2() -> Vec<Span> {
    let gitea = "gitea";
    vec![
        mock(
            0,
            None,
            PROXY,
            "GET /homelab/stacks/src/branch/main",
            0.0,
            142.3,
            false,
            None,
        ),
        mock(1, Some(0), PROXY, "upstream gitea", 0.5, 141.2, false, None),
        mock(
            2,
            Some(1),
            gitea,
            "GET /{owner}/{repo}/src/branch/*",
            2.9,
            137.6,
            false,
            None,
        ),
        mock(3, Some(2), gitea, "context.Auth", 3.6, 2.8, false, None),
        mock(4, Some(3), gitea, "SELECT user", 4.0, 1.6, false, pg()),
        mock(5, Some(2), gitea, "repo.Load", 6.9, 21.8, false, None),
        mock(
            6,
            Some(5),
            gitea,
            "SELECT repository",
            7.3,
            2.1,
            false,
            pg(),
        ),
        mock(
            7,
            Some(5),
            gitea,
            "git rev-parse",
            10.2,
            15.9,
            false,
            exec(),
        ),
        mock(8, Some(2), gitea, "repo.Tree", 29.1, 84.6, false, None),
        mock(9, Some(8), gitea, "git ls-tree", 29.8, 40.7, false, exec()),
        mock(
            10,
            Some(8),
            gitea,
            "git log -1 (×18)",
            71.2,
            41.9,
            false,
            exec(),
        ),
        mock(
            11,
            Some(2),
            gitea,
            "template.Render repo/home",
            114.3,
            23.8,
            false,
            None,
        ),
    ]
}

fn params(pairs: &[(&str, &str)]) -> TraceParams {
    TraceParams::from(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<Vec<_>>(),
    )
}

/// The waterfall of `spans` (of which there are `total`), selected as the list selects it.
fn waterfall(spans: Vec<Span>, total: u64, palette: &ServicePalette) -> Waterfall {
    let id = trace_id().to_string();
    let trace = Trace::new(trace_id(), spans, total);
    Waterfall::new(trace, palette, &params(&[("trace", &id)]))
}

fn palette(services: &[&str]) -> ServicePalette {
    let mut palette = ServicePalette::default();
    palette.learn(services.iter().copied());
    palette
}

/// Each row's bar as (left, width, label side).
fn bars(waterfall: &Waterfall) -> Vec<(f64, f64, LabelSide)> {
    waterfall
        .rows
        .iter()
        .map(|row| (row.bar.left, row.bar.width, row.bar.side))
        .collect()
}

#[test]
fn the_header_of_t1_has_its_figures_legend_and_axis() {
    let waterfall = waterfall(t1(), 12, &palette(&["outline-app"]));
    assert_eq!(waterfall.stack, "outline");
    assert_eq!(waterfall.title, "POST /api/documents.info");
    assert_eq!(
        waterfall.started,
        Some((
            "2026-11-12T19:58:41.081Z".to_owned(),
            "2026-11-12 19:58:41.081".to_owned()
        ))
    );
    assert_eq!(waterfall.duration, "1.91 s");
    assert_eq!(waterfall.spans, "12");
    assert_eq!(waterfall.depth, 5);
    assert_eq!(waterfall.errors, 5);
    assert_eq!(
        waterfall.legend,
        [
            LegendEntry {
                name: "hoster".into(),
                class: "svc-hoster",
                count: "2 spans".into()
            },
            LegendEntry {
                name: "outline-app".into(),
                class: "svc-0",
                count: "10 spans".into()
            },
        ]
    );
    assert_eq!(
        waterfall.ticks,
        ["0 µs", "478 ms", "956 ms", "1.43 s", "1.91 s"]
    );
    assert_eq!(waterfall.more(), None);
}

#[test]
fn rows_of_t1_are_depth_first_indented_by_depth_with_errors_and_tags() {
    let waterfall = waterfall(t1(), 12, &palette(&["outline-app"]));
    let rows: Vec<(&str, &str, bool, Option<&str>)> = waterfall
        .rows
        .iter()
        .map(|r| (r.name.as_str(), r.depth_class, r.error, r.tag.as_deref()))
        .collect();
    assert_eq!(
        rows,
        [
            ("POST /api/documents.info", "wf-depth-0", true, None),
            ("upstream outline", "wf-depth-1", true, None),
            ("POST /api/documents.info", "wf-depth-2", true, None),
            ("authenticate", "wf-depth-3", false, None),
            ("GET", "wf-depth-4", false, Some("redis")),
            ("SELECT users", "wf-depth-4", false, Some("pg")),
            ("documents.info", "wf-depth-3", true, None),
            ("GET", "wf-depth-4", false, Some("redis")),
            ("SELECT documents", "wf-depth-4", false, Some("pg")),
            ("SELECT collections", "wf-depth-4", false, Some("pg")),
            ("SELECT revisions", "wf-depth-4", true, Some("pg")),
            ("ROLLBACK", "wf-depth-4", false, Some("pg")),
        ]
    );
    let classes: Vec<&str> = waterfall.rows.iter().map(|r| r.class).collect();
    assert_eq!(classes[..3], ["svc-hoster", "svc-hoster", "svc-0"]);
    assert!(waterfall.rows.iter().all(|r| !r.parent_missing));
    let s9 = &waterfall.rows[10];
    assert_eq!(
        (s9.took.as_str(), s9.offset.as_str()),
        ("1.80 s", "+51.7 ms")
    );
    let s12 = &waterfall.rows[7];
    assert_eq!(
        (s12.took.as_str(), s12.offset.as_str()),
        ("900 µs", "+24.6 ms")
    );
}

#[test]
fn bars_of_t1_sit_on_one_axis_with_their_labels_inside_after_or_before() {
    use LabelSide::{Inside, Left, Right};
    let waterfall = waterfall(t1(), 12, &palette(&["outline-app"]));
    assert_eq!(
        bars(&waterfall),
        [
            (0.0, 1000.0, Inside),
            (0.3, 999.2, Inside),
            (2.0, 996.2, Inside),
            (2.7, 9.2, Right),
            // 1.8 ms and 0.9 ms are narrower than the minimum.
            (3.6, 3.0, Right),
            (5.4, 4.8, Right),
            (12.5, 983.2, Inside),
            (12.9, 3.0, Right),
            (13.7, 7.4, Right),
            (21.4, 4.3, Right),
            (27.0, 941.4, Inside),
            // Near the right edge: the label goes before the bar.
            (969.3, 3.0, Left),
        ]
    );
}

#[test]
fn bars_of_t2_put_labels_before_bars_that_end_late() {
    use LabelSide::{Inside, Left, Right};
    let waterfall = waterfall(t2(), 12, &palette(&["gitea"]));
    assert_eq!(
        bars(&waterfall),
        [
            (0.0, 1000.0, Inside),
            (3.5, 992.3, Inside),
            (20.4, 967.0, Inside),
            (25.3, 19.7, Right),
            (28.1, 11.2, Right),
            (48.5, 153.2, Right),
            (51.3, 14.8, Right),
            (71.7, 111.7, Right),
            // 59 % wide: inside, though it ends past the right-edge mark.
            (204.5, 594.5, Inside),
            (209.4, 286.0, Right),
            (500.4, 294.4, Left),
            (803.2, 167.3, Left),
        ]
    );
    let tags: Vec<Option<&str>> = waterfall.rows.iter().map(|r| r.tag.as_deref()).collect();
    assert_eq!(
        tags,
        [
            None,
            None,
            None,
            None,
            Some("pg"),
            None,
            Some("pg"),
            Some("exec"),
            None,
            Some("exec"),
            Some("exec"),
            None
        ]
    );
    assert_eq!(waterfall.errors, 0);
    assert_eq!(waterfall.depth, 5);
    assert_eq!(waterfall.legend[1].class, "svc-0");
}

#[test]
fn a_bar_is_drawn_as_percentages_with_its_label_and_an_error_ring() {
    let waterfall = waterfall(t1(), 12, &palette(&["outline-app"]));
    let s9 = waterfall.rows[10].bar.to_string();
    assert_eq!(
        s9,
        concat!(
            r#"<svg class="wf-bar" width="100%" height="30" aria-hidden="true" focusable="false">"#,
            r#"<line class="wf-grid" x1="25%" y1="0" x2="25%" y2="30"></line>"#,
            r#"<line class="wf-grid" x1="50%" y1="0" x2="50%" y2="30"></line>"#,
            r#"<line class="wf-grid" x1="75%" y1="0" x2="75%" y2="30"></line>"#,
            r#"<rect class="wf-ring" x="2.70%" y="8" width="94.14%" height="14" rx="3"></rect>"#,
            r#"<rect class="wf-gap" x="2.70%" y="8" width="94.14%" height="14" rx="3"></rect>"#,
            r#"<rect class="wf-fill svc-0" x="2.70%" y="8" width="94.14%" height="14" rx="3"></rect>"#,
            r#"<text class="wf-took wf-took-inside" x="2.70%" dx="8" y="50%">1.80 s</text></svg>"#,
        )
    );
    // No ring without an error; the label after the bar, or before it.
    let s3 = waterfall.rows[3].bar.to_string();
    assert!(!s3.contains("wf-ring"), "{s3}");
    assert!(
        s3.contains(
            r#"<text class="wf-took wf-took-right" x="1.19%" dx="6" y="50%">17.6 ms</text>"#
        ),
        "{s3}"
    );
    let s10 = waterfall.rows[11].bar.to_string();
    assert!(
        s10.contains(
            r#"<text class="wf-took wf-took-left" x="96.93%" dx="-6" y="50%">2.7 ms</text>"#
        ),
        "{s10}"
    );
}

#[test]
fn rows_link_to_their_span_and_the_selected_one_is_marked() {
    let trace = Trace::new(trace_id(), t1(), 12);
    let id = trace_id().to_string();
    let s9 = SpanId::from([9; 8]).to_string();
    let waterfall = Waterfall::new(
        trace,
        &palette(&["outline-app"]),
        &params(&[
            ("trace", &id),
            ("page", "1"),
            ("status", "error"),
            ("span", &s9.to_ascii_uppercase()),
        ]),
    );
    let row = &waterfall.rows[10];
    assert!(row.current);
    assert_eq!(waterfall.rows.iter().filter(|r| r.current).count(), 1);
    assert_eq!(
        waterfall.rows[3].href,
        format!(
            "/telemetry/traces?status=error&page=1&trace={id}&span={}",
            SpanId::from([3; 8])
        )
    );
    // A span that is not an id selects none.
    let trace = Trace::new(trace_id(), t1(), 12);
    let waterfall = Waterfall::new(
        trace,
        &ServicePalette::default(),
        &params(&[("trace", &id), ("span", "s9")]),
    );
    assert!(waterfall.rows.iter().all(|r| !r.current));
}

#[test]
fn a_partial_trace_hangs_lost_spans_under_the_root_and_says_what_is_not_shown() {
    // s2 lost: s3 and s6 hang under the root, among its children by start; 15 spans in all.
    let spans: Vec<Span> = t1().into_iter().filter(|s| s.span_id != id(2)).collect();
    let waterfall = waterfall(spans, 15, &palette(&["outline-app"]));
    let rows: Vec<(&str, usize, bool)> = waterfall
        .rows
        .iter()
        .map(|r| (r.name.as_str(), r.depth, r.parent_missing))
        .collect();
    assert_eq!(
        rows[..5],
        [
            ("POST /api/documents.info", 0, false),
            ("upstream outline", 1, false),
            ("authenticate", 1, true),
            ("GET", 2, false),
            ("SELECT users", 2, false),
        ]
    );
    assert_eq!(rows[5], ("documents.info", 1, true));
    assert_eq!(waterfall.spans, "15");
    assert_eq!(
        waterfall.more().as_deref(),
        Some("The first 11 spans by start: 4 more not shown.")
    );
    assert_eq!(waterfall.minimap.lines.len(), 11);
}

#[test]
fn deep_rows_are_indented_to_the_cap() {
    let spans: Vec<Span> = (0..14u8)
        .map(|n| {
            mock(
                n,
                n.checked_sub(1),
                "worker",
                "step",
                f64::from(n),
                100.0 - f64::from(n),
                false,
                None,
            )
        })
        .collect();
    let waterfall = waterfall(spans, 14, &ServicePalette::default());
    let classes: Vec<&str> = waterfall.rows.iter().map(|r| r.depth_class).collect();
    assert_eq!(classes[9], "wf-depth-9");
    assert_eq!(classes[10], "wf-depth-10");
    assert_eq!(classes[13], "wf-depth-10");
    assert_eq!(waterfall.rows[13].depth, 13);
}

#[test]
fn the_minimap_draws_every_span_as_a_line_in_its_colour_or_red() {
    let waterfall = waterfall(t1(), 12, &palette(&["outline-app"]));
    let svg = waterfall.minimap.to_string();
    assert!(
        svg.starts_with(r#"<svg class="wf-minimap" viewBox="0 0 1000 42" preserveAspectRatio="none" width="100%" height="42" aria-hidden="true" focusable="false"><rect class="wf-mini wf-mini-error" x="0" y="3" width="1000" height="2"></rect>"#),
        "{svg}"
    );
    assert_eq!(svg.matches("<rect ").count(), 12);
    // s3, the fourth row; s12, 0.9 ms, at the minimum width.
    assert!(
        svg.contains(
            r#"<rect class="wf-mini svc-0" x="2.7" y="12" width="9.2" height="2"></rect>"#
        ),
        "{svg}"
    );
    assert!(
        svg.contains(r#"<rect class="wf-mini svc-0" x="12.9" y="24" width="4" height="2"></rect>"#),
        "{svg}"
    );

    // A long trace is squeezed into the tallest minimap.
    let spans: Vec<Span> = (1..=200u8)
        .map(|n| mock(n, None, "worker", "tick", f64::from(n), 1.0, false, None))
        .collect();
    let svg = waterfall_of(spans).minimap.to_string();
    assert!(
        svg.contains(
            r#"viewBox="0 0 1000 606" preserveAspectRatio="none" width="100%" height="120""#
        ),
        "{svg}"
    );
}

fn waterfall_of(spans: Vec<Span>) -> Waterfall {
    let total = spans.len() as u64;
    waterfall(spans, total, &ServicePalette::default())
}

#[test]
fn a_trace_of_no_length_draws_every_bar_at_its_start() {
    let waterfall = waterfall_of(vec![
        mock(1, None, "worker", "tick", 0.0, 0.0, false, None),
        mock(2, Some(1), "worker", "tock", 0.0, 0.0, false, None),
    ]);
    assert_eq!(
        bars(&waterfall),
        [(0.0, 3.0, LabelSide::Right), (0.0, 3.0, LabelSide::Right)]
    );
    assert_eq!(waterfall.duration, "0 µs");
    assert_eq!(waterfall.legend[0].count, "2 spans");
}

#[test]
fn tags_come_from_the_database_or_process_a_span_calls() {
    let tag = |attributes: serde_json::Value| {
        let span = mock(1, None, "api", "x", 0.0, 1.0, false, Some(attributes));
        SpanRow::tag(&span)
    };
    assert_eq!(
        tag(json!({ "db.system.name": "postgresql" })).as_deref(),
        Some("pg")
    );
    assert_eq!(
        tag(json!({ "db.system": "mysql" })).as_deref(),
        Some("mysql")
    );
    assert_eq!(
        tag(json!({ "db.system": "a-database-with-a-long-name" })).as_deref(),
        Some("db")
    );
    assert_eq!(
        tag(json!({ "process.command": "git" })).as_deref(),
        Some("exec")
    );
    assert_eq!(tag(json!({ "http.route": "/" })), None);
    let none = mock(1, None, "api", "x", 0.0, 1.0, false, None);
    assert_eq!(SpanRow::tag(&none), None);
}

#[test]
fn span_times_are_microseconds_below_a_millisecond() {
    let shown = |us: u64| SpanTime(Duration::from_micros(us)).to_string();
    assert_eq!(shown(0), "0 µs");
    assert_eq!(shown(40), "40 µs");
    assert_eq!(shown(999), "999 µs");
    assert_eq!(shown(1_800), "1.8 ms");
    assert_eq!(shown(17_600), "17.6 ms");
    assert_eq!(shown(478_600), "479 ms");
    assert_eq!(shown(1_912_400), "1.91 s");
    assert_eq!(shown(185_000_000), "3 min 5 s");
    // The unit is picked after rounding.
    let nanos = |ns: u64| SpanTime(Duration::from_nanos(ns)).to_string();
    assert_eq!(nanos(999_600), "1.0 ms");
    assert_eq!(nanos(99_960_000), "100 ms");
    assert_eq!(nanos(999_600_000), "1.00 s");
    assert_eq!(nanos(59_996_000_000), "1 min");
    assert_eq!(nanos(99_940_000), "99.9 ms");
    assert_eq!(nanos(185_600_000_000), "3 min 6 s");
}

/// The mockup's `t1` with `s9` as the mockup has it: the statement timeout with its status
/// message, attributes, resource and exception event.
fn t1_detailed() -> Vec<Span> {
    let mut spans = t1();
    let s9 = spans.iter_mut().find(|s| s.span_id == id(9)).unwrap();
    s9.kind = SpanKind::Client;
    s9.status_message = Some("canceling statement due to statement timeout".into());
    s9.attributes = Some(
        json!({
            "db.system": "postgresql",
            "db.namespace": "outline",
            "db.query.text": "SELECT r.* FROM revisions r WHERE r.document_id = $1 ORDER BY r.created_at DESC",
            "db.response.status_code": "57014",
            "error.type": "QueryCanceledError",
            "server.address": "postgres:5432",
        })
        .to_string(),
    );
    s9.resource = Some(
        json!({
            "service.name": "outline-app",
            "service.version": "0.81.1",
            "telemetry.sdk.language": "nodejs",
            "telemetry.sdk.name": "opentelemetry",
            "service.namespace": "outline",
            "hoster.generation": "4",
            "host.name": "srv-01",
        })
        .to_string(),
    );
    s9.events = Some(
        json!([{
            "name": "exception",
            "time_us": BASE_US + 1_852_100,
            "attributes": {
                "exception.type": "SequelizeDatabaseError",
                "exception.message": "canceling statement due to statement timeout",
                "exception.stacktrace": "at Query.formatError (node_modules/sequelize/lib/dialects/postgres/query.js:386:16)\nat Query.run (node_modules/sequelize/lib/dialects/postgres/query.js:87:18)",
            },
        }])
        .to_string(),
    );
    spans
}

fn details_of(spans: Vec<Span>, pairs: &[(&str, &str)]) -> Waterfall {
    let id = trace_id().to_string();
    let mut given = vec![("trace", id.as_str())];
    given.extend_from_slice(pairs);
    Waterfall::new(
        Trace::new(trace_id(), spans, 12),
        &palette(&["outline-app"]),
        &params(&given),
    )
}

#[test]
fn t1_opens_on_its_failing_statement_with_its_details() {
    let waterfall = details_of(t1_detailed(), &[("page", "1")]);
    // No `span`: the end of the first error chain, s9, is selected.
    let selected: Vec<&str> = waterfall
        .rows
        .iter()
        .filter(|r| r.current)
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(selected, ["SELECT revisions"]);
    let trace = trace_id();
    // Its row's link closes the details, as the close link does.
    let close = format!("/telemetry/traces?page=1&trace={trace}&span=none");
    assert_eq!(waterfall.rows[10].href, close);

    let details = waterfall.details.as_ref().unwrap();
    assert_eq!(details.id, id(9));
    assert_eq!(
        (details.name.as_str(), details.service.as_str()),
        ("SELECT revisions", "outline-app")
    );
    assert_eq!(details.kind, SpanKind::Client);
    assert_eq!(
        (details.status, details.status_class),
        (SpanStatus::Error, "pill-fail")
    );
    assert_eq!(details.close, close);
    assert_eq!(details.start, "+51.7 ms");
    assert_eq!(details.duration, "1.80 s");
    assert_eq!(details.share.as_deref(), Some("94.1%"));
    assert_eq!(details.self_time, "1.80 s");
    assert_eq!(details.parent, id(6).to_string());
    assert_eq!(
        details.parent_href.as_deref(),
        Some(format!("/telemetry/traces?page=1&trace={trace}&span={}", id(6)).as_str())
    );
    assert!(!details.parent_missing);
    assert_eq!(
        details.message.as_deref(),
        Some("canceling statement due to statement timeout")
    );

    let attributes: Vec<(&str, &str, bool)> = details
        .attributes
        .iter()
        .map(|a| (a.key.as_str(), a.value.as_str(), a.alert))
        .collect();
    assert_eq!(
        attributes,
        [
            ("db.namespace", "outline", false),
            (
                "db.query.text",
                "SELECT r.* FROM revisions r WHERE r.document_id = $1 ORDER BY r.created_at DESC",
                false
            ),
            ("db.response.status_code", "57014", false),
            ("db.system", "postgresql", false),
            ("error.type", "QueryCanceledError", true),
            ("server.address", "postgres:5432", false),
        ]
    );
    assert_eq!(details.attributes_raw, None);
    // The named resource attributes, in their order; the rest are left out.
    let resource: Vec<(&str, &str)> = details
        .resource
        .iter()
        .map(|c| (c.key.as_str(), c.value.as_str()))
        .collect();
    assert_eq!(
        resource,
        [
            ("service.name", "outline-app"),
            ("service.version", "0.81.1"),
            ("telemetry.sdk.language", "nodejs"),
            ("service.namespace", "outline"),
            ("hoster.generation", "4"),
        ]
    );

    let [event] = details.events.as_slice() else {
        panic!("{:?}", details.events)
    };
    assert_eq!(event.name, "exception");
    assert!(event.exception);
    assert_eq!(event.at, "@ 1.85 s (+1.80 s in span)");
    assert_eq!(
        event.attributes,
        [
            (
                "exception.message".to_owned(),
                "canceling statement due to statement timeout".to_owned()
            ),
            (
                "exception.type".to_owned(),
                "SequelizeDatabaseError".to_owned()
            ),
        ]
    );
    assert_eq!(
        event.stacktrace.as_deref(),
        Some(
            "at Query.formatError (node_modules/sequelize/lib/dialects/postgres/query.js:386:16)\nat Query.run (node_modules/sequelize/lib/dialects/postgres/query.js:87:18)"
        )
    );
    assert_eq!(details.logs, SpanLogs::default());
}

#[test]
fn self_time_is_the_duration_less_the_union_of_its_children() {
    // documents.info (1880.3 ms) less its five children (1826.4 ms, none overlapping).
    let waterfall = details_of(t1(), &[("span", &id(6).to_string())]);
    let details = waterfall.details.unwrap();
    assert_eq!(details.name, "documents.info");
    assert_eq!(details.self_time, "53.9 ms");
    assert_eq!(details.start, "+23.9 ms");
    assert_eq!(details.share.as_deref(), Some("98.3%"));
    // authenticate (17.6 ms) less GET and SELECT users (11.0 ms).
    let waterfall = details_of(t1(), &[("span", &id(3).to_string())]);
    assert_eq!(waterfall.details.unwrap().self_time, "6.6 ms");
}

#[test]
fn the_root_has_no_parent_and_a_span_without_anything_says_so() {
    let waterfall = details_of(t1(), &[("span", &id(0).to_string())]);
    let details = waterfall.details.unwrap();
    assert_eq!(details.parent, "— (root)");
    assert_eq!(details.parent_href, None);
    assert_eq!(details.start, "+0 µs");
    assert_eq!(details.share.as_deref(), Some("100.0%"));
    // In error without a message: no status line.
    assert_eq!(details.message, None);
    assert!(details.attributes.is_empty());
    assert!(details.resource.is_empty());
    assert!(details.events.is_empty());
    // A span not in error has no status line even with a message.
    let mut spans = t1();
    spans[3].status_message = Some("fine".into());
    let waterfall = details_of(spans, &[("span", &id(3).to_string())]);
    let details = waterfall.details.unwrap();
    assert_eq!(
        (details.status, details.status_class, details.message),
        (SpanStatus::Unset, "pill-neutral", None)
    );
}

#[test]
fn closing_selects_nothing_and_a_trace_without_errors_opens_on_its_root() {
    let waterfall = details_of(t1(), &[("span", TraceParams::NO_SPAN)]);
    assert!(waterfall.details.is_none());
    assert!(waterfall.rows.iter().all(|r| !r.current));
    // A span id the trace does not have selects none either.
    let waterfall = details_of(t1(), &[("span", &id(0x42).to_string())]);
    assert!(waterfall.details.is_none());
    // Nothing failed: the root.
    let waterfall = details_of(t2(), &[]);
    assert_eq!(waterfall.details.unwrap().id, id(0));
    assert!(waterfall.rows[0].current);
}

#[test]
fn a_lost_parent_is_named_but_not_linked() {
    let spans: Vec<Span> = t1().into_iter().filter(|s| s.span_id != id(2)).collect();
    let waterfall = details_of(spans, &[("span", &id(6).to_string())]);
    let details = waterfall.details.unwrap();
    assert_eq!(details.parent, id(2).to_string());
    assert_eq!(details.parent_href, None);
    assert!(details.parent_missing);
}

#[test]
fn the_proxys_spans_have_their_attributes_and_an_answer_of_5xx_in_red() {
    let alert = |key: &str, value: serde_json::Value| {
        let mut spans = t1();
        spans[0].attributes = Some(json!({ key: value }).to_string());
        let waterfall = details_of(spans, &[("span", &id(0).to_string())]);
        waterfall.details.unwrap().attributes[0].alert
    };
    assert!(alert("http.response.status_code", json!(500)));
    assert!(alert("http.response.status_code", json!("503")));
    assert!(!alert("http.response.status_code", json!(404)));
    assert!(!alert("http.response.status_code", json!(600)));
    assert!(alert("error.type", json!("timeout")));
    assert!(!alert("http.request.method", json!("POST")));
    // Values that are not text are shown as JSON; a column that is not an object as it is.
    let mut spans = t1();
    spans[0].attributes = Some(json!({ "tags": ["a", 1], "n": 2.5, "ok": true }).to_string());
    spans[1].attributes = Some("not json".into());
    spans[1].resource = Some("[]".into());
    spans[1].events = Some(r#"{"name": "x"}"#.into());
    let waterfall = details_of(spans.clone(), &[("span", &id(0).to_string())]);
    let values: Vec<(String, String)> = waterfall
        .details
        .unwrap()
        .attributes
        .into_iter()
        .map(|a| (a.key, a.value))
        .collect();
    assert_eq!(
        values,
        [
            ("n".to_owned(), "2.5".to_owned()),
            ("ok".to_owned(), "true".to_owned()),
            ("tags".to_owned(), r#"["a",1]"#.to_owned()),
        ]
    );
    let waterfall = details_of(spans, &[("span", &id(1).to_string())]);
    let details = waterfall.details.unwrap();
    assert!(details.attributes.is_empty());
    assert_eq!(details.attributes_raw.as_deref(), Some("not json"));
    assert!(details.resource.is_empty());
    assert!(details.events.is_empty());
}

#[test]
fn events_are_timed_from_the_trace_and_the_span_and_need_no_attributes() {
    let mut spans = t1();
    spans[3].events = Some(
        json!([
            { "name": "cache miss", "time_us": BASE_US + 6_100 },
            "not an event",
            { "name": "early", "time_us": BASE_US - 5_000 },
        ])
        .to_string(),
    );
    let waterfall = details_of(spans, &[("span", &id(3).to_string())]);
    let events = waterfall.details.unwrap().events;
    let shown: Vec<(&str, &str, bool, usize)> = events
        .iter()
        .map(|e| {
            (
                e.name.as_str(),
                e.at.as_str(),
                e.exception,
                e.attributes.len(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("cache miss", "@ 6.1 ms (+1.0 ms in span)", false, 0),
            // Before the trace began: at its start.
            ("early", "@ 0 µs (+0 µs in span)", false, 0),
        ]
    );
    assert_eq!(events[0].stacktrace, None);
}

fn entry(n: i64, ms: i64, stack: Option<&str>, message: &str) -> LogEntry {
    LogEntry {
        id: n,
        record: crate::store::LogRecord {
            time: Timestamp::from_millis(BASE_US / 1000 + ms),
            stack: stack.map(str::to_owned),
            service: "outline-app".into(),
            generation: Some(4),
            stream: crate::store::Stream::Otlp,
            level: Some(crate::store::Level::Error),
            message: message.into(),
            fields: None,
            cut: false,
            marker: false,
            trace_id: Some(trace_id()),
            span_id: Some(id(9)),
            received: None,
        },
    }
}

#[test]
fn span_logs_link_each_record_to_the_logs_tab_and_say_when_there_are_more() {
    let logs = SpanLogs::from(vec![
        entry(41, 1_852, Some("outline"), "query failed"),
        entry(42, 1_853, None, "hoster's own"),
    ]);
    assert_eq!(logs.more, None);
    assert!(!logs.unreadable);
    let first = &logs.records[0];
    assert_eq!(first.time, "2026-11-12T19:58:42.933Z");
    assert_eq!(first.shown, "19:58:42.933");
    assert_eq!(first.level, Some(crate::store::Level::Error));
    assert_eq!(first.message, "query failed");
    // The record is the first of the page after the one before it, in a range from it.
    assert_eq!(
        first.href,
        "/telemetry/logs?stack=outline&since=2026-11-12T19%3A58%3A42.933Z&after=1794513522933.40"
    );
    assert!(
        logs.records[1]
            .href
            .starts_with("/telemetry/logs?stack=_hoster&"),
        "{}",
        logs.records[1].href
    );

    let limit = SpanLogs::LIMIT as i64;
    let many: Vec<LogEntry> = (0..=limit)
        .map(|n| entry(100 + n, n, Some("outline"), "line"))
        .collect();
    let next = SpanLog::from(&many[SpanLogs::LIMIT as usize]).href;
    let logs = SpanLogs::from(many);
    assert_eq!(logs.records.len(), SpanLogs::LIMIT as usize);
    // "the rest" starts at the first record not shown: the 51st.
    assert_eq!(logs.more, Some(next));
    assert!(
        logs.more
            .as_deref()
            .unwrap()
            .ends_with("&after=1794513521131.149"),
        "{:?}",
        logs.more
    );
    assert_eq!(logs.first(), "the first 50 records");
}
