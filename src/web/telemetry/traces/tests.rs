//! The Traces tab through the whole app, against seeded request rows and spans and the fake
//! orchestrator, whose state holds the stack `notes`; and its parameters and palette alone.

use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;

use axum::http::{StatusCode, header};

use super::*;
use crate::store::{
    LogBatch, LogRecord, MicroTimestamp, RequestRow, SpanId, SpanKind, SpanStatus, Stream,
};
use crate::web::auth::tests::TestApp;
use crate::web::stacks::tests::body;

fn trace(n: u8) -> TraceId {
    TraceId::from([n; 16])
}

/// Span `k` of trace `n`: the proxy's are 0 (server) and 1 (upstream client).
fn sid(n: u8, k: u8) -> SpanId {
    SpanId::from([n, k, 0, 0, 0, 0, 0, 1])
}

/// The request row of trace `n`, `ago` seconds ago, with the proxy's span ids and an upstream
/// start 1 ms after its arrival.
fn request(n: u8, ago: u64, stack: &str, path: &str, status: u16, total_ms: u64) -> RequestRow {
    let time = Timestamp::now() - Duration::from_secs(ago);
    RequestRow {
        time,
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
        server_span: Some(sid(n, 0)),
        client_span: Some(sid(n, 1)),
        upstream_start: Some(MicroTimestamp::from(time + Duration::from_millis(1))),
    }
}

/// Stored span `id` of trace `n` under span `parent` of it, started `ago_ms` milliseconds ago.
#[allow(
    clippy::too_many_arguments,
    reason = "a test fixture with every field that varies"
)]
fn span(
    n: u8,
    id: u8,
    parent: Option<u8>,
    service: &str,
    name: &str,
    ago_ms: u64,
    duration_ms: u64,
    status: SpanStatus,
) -> Span {
    Span {
        trace_id: trace(n),
        span_id: sid(n, id),
        parent: parent.map(|parent| sid(n, parent)),
        stack: "notes".into(),
        service: service.into(),
        name: name.into(),
        kind: SpanKind::Internal,
        start: MicroTimestamp::from(Timestamp::now() - Duration::from_millis(ago_ms)),
        duration: Duration::from_millis(duration_ms),
        status,
        status_message: None,
        attributes: None,
        resource: None,
        events: None,
        received: Timestamp::now(),
    }
}

fn record(ago: u64, message: &str) -> LogRecord {
    LogRecord {
        time: Timestamp::now() - Duration::from_secs(ago),
        stack: Some("notes".into()),
        service: "web".into(),
        generation: Some(1),
        stream: Stream::Stdout,
        level: None,
        message: message.into(),
        fields: None,
        cut: false,
        marker: false,
        trace_id: None,
        span_id: None,
        received: None,
    }
}

/// In the last 15 minutes:
/// - trace 1: `GET /a` to `notes` a minute ago (200, 50 ms), whose app exported a span and a
///   child of another service that failed;
/// - trace 2: `GET /b` to `notes` two minutes ago (200, 300 ms), whose app exports nothing;
/// - trace 3: `GET /c` to `blog` three minutes ago that hoster answered itself (503, 10 ms);
/// - trace 5: a job `notes`' worker started four minutes ago (2 s).
///
/// And trace 9, `GET /old` two hours ago; two log records in the last 15 minutes and one
/// outside them.
async fn seeded() -> TestApp {
    let app = TestApp::new().await;
    let mut answered = request(3, 180, "blog", "/c", 503, 10);
    answered.client_span = None;
    answered.upstream_start = None;
    app.logs
        .append(&LogBatch {
            records: vec![
                record(3_000, "fifty minutes ago"),
                record(100, "started"),
                record(50, "listening"),
            ],
            requests: vec![
                request(9, 7_200, "notes", "/old", 200, 5),
                request(1, 60, "notes", "/a", 200, 50),
                request(2, 120, "notes", "/b", 200, 300),
                answered,
            ],
            spans: vec![
                span(
                    1,
                    2,
                    Some(1),
                    "web",
                    "GET /a",
                    59_998,
                    45,
                    SpanStatus::Unset,
                ),
                span(
                    1,
                    3,
                    Some(2),
                    "api",
                    "lookup",
                    59_990,
                    20,
                    SpanStatus::Error,
                ),
                span(
                    5,
                    2,
                    None,
                    "worker",
                    "nightly",
                    240_000,
                    2_000,
                    SpanStatus::Unset,
                ),
                span(
                    5,
                    3,
                    Some(2),
                    "worker",
                    "sweep",
                    239_900,
                    500,
                    SpanStatus::Unset,
                ),
            ],
            ..LogBatch::default()
        })
        .await
        .unwrap();
    app
}

/// The root span names of the rows, in page order.
fn names(html: &str) -> Vec<&str> {
    html.split(r#"<td class="trace-name"><a href=""#)
        .skip(1)
        .map(|rest| {
            let rest = &rest[rest.find('>').unwrap() + 1..];
            rest.split("</a>").next().unwrap()
        })
        .collect()
}

/// The row of `name`.
fn row<'a>(html: &'a str, name: &str) -> &'a str {
    let at = html
        .find(&format!(">{name}</a></td>"))
        .unwrap_or_else(|| panic!("no row {name}: {html}"));
    let start = html[..at].rfind("<tr").unwrap();
    let end = at + html[at..].find("</tr>").unwrap();
    &html[start..end]
}

/// The href of the row `name`'s root span link, unescaped.
fn select_href(html: &str, name: &str) -> String {
    let row = row(html, name);
    let rest = row
        .split(r#"<td class="trace-name"><a href=""#)
        .nth(1)
        .unwrap();
    rest.split('"').next().unwrap().replace("&#38;", "&")
}

struct Session {
    app: TestApp,
    cookie: String,
}

impl Session {
    async fn new() -> Self {
        let app = seeded().await;
        let cookie = app.sign_in().await.cookie;
        Self { app, cookie }
    }

    async fn get(&self, uri: &str) -> String {
        let response = self.app.get(uri, Some(&self.cookie)).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        body(response).await
    }
}

#[tokio::test]
async fn the_page_redirects_to_its_traces_tab() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app.get("/telemetry", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/telemetry/traces");
}

#[tokio::test]
async fn the_list_shows_the_ranges_traces_newest_first_with_every_column() {
    let session = Session::new().await;
    let html = session.get("/telemetry/traces").await;
    assert_eq!(
        names(&html),
        ["GET /a", "GET /b", "GET /c", "nightly"],
        "{html}"
    );
    assert!(
        html.contains(r#"Traces <span class="card-count">· 4 of 4 · newest first</span></h2>"#),
        "{html}"
    );

    // Trace 1: the proxy's spans as `hoster` in its colour, then the app's services in turn.
    let one = row(&html, "GET /a");
    let services: Vec<&str> = one
        .split(r#"<span class="trace-service"><span class="dot "#)
        .skip(1)
        .map(|rest| &rest[..rest.match_indices("</span>").nth(1).unwrap().0])
        .collect();
    assert_eq!(
        services,
        [
            r#"svc-hoster" aria-hidden="true"></span>hoster"#,
            r#"svc-0" aria-hidden="true"></span>web"#,
            r#"svc-1" aria-hidden="true"></span>api"#,
        ],
        "{one}"
    );
    assert!(one.contains(r#"<td class="num">4</td>"#), "{one}");
    assert!(
        one.contains(r#"<span class="trace-took">50.0 ms</span>"#),
        "{one}"
    );
    // Written as the scatter and the waterfall write durations.
    for (name, took) in [
        ("GET /b", "300 ms"),
        ("GET /c", "10.0 ms"),
        ("nightly", "2.00 s"),
    ] {
        assert!(
            row(&html, name).contains(&format!(r#"<span class="trace-took">{took}</span>"#)),
            "{name}: {html}"
        );
    }
    // In error by its app's span, though the request answered 200: red bar and status.
    assert!(one.contains(r#"class="meter meter-fail""#), "{one}");
    assert!(
        one.contains(r#"<span class="trace-status trace-error"><span class="dot" aria-hidden="true"></span>error</span>"#),
        "{one}"
    );
    assert!(
        one.contains(r#"<td class="trace-stack">notes</td>"#),
        "{one}"
    );
    assert!(!one.contains("app not instrumented"), "{one}");

    // Trace 2: the app was called and sent nothing.
    let two = row(&html, "GET /b");
    assert!(
        two.contains(r#"<span class="trace-uninstrumented">app not instrumented</span>"#),
        "{two}"
    );
    assert!(two.contains(r#"<td class="num">2</td>"#), "{two}");
    assert!(two.contains(r#"class="meter meter-neutral""#), "{two}");
    assert!(two.contains(r#"trace-status trace-ok"#), "{two}");
    // Trace 3: hoster answered itself, so no app was called: no label.
    let three = row(&html, "GET /c");
    assert!(three.contains(r#"<td class="num">1</td>"#), "{three}");
    assert!(!three.contains("app not instrumented"), "{three}");
    assert!(
        three.contains(r#"<td class="trace-stack">blog</td>"#),
        "{three}"
    );
    // The job has the worker's colour and no proxy.
    let job = row(&html, "nightly");
    assert!(
        job.contains(r#"svc-2" aria-hidden="true"></span>worker"#),
        "{job}"
    );
    assert!(!job.contains("svc-hoster"), "{job}");

    // The bar is scaled to the slowest listed (the job, 2 s): full for it, at least 2 %.
    assert!(
        job.contains(r#"<rect class="meter-fill" width="100" height="6">"#),
        "{job}"
    );
    assert!(
        two.contains(r#"<rect class="meter-fill" width="15" height="6">"#),
        "{two}"
    );
    assert!(
        three.contains(r#"<rect class="meter-fill" width="2" height="6">"#),
        "{three}"
    );
    // The time of day, to the millisecond, with the full time in `datetime`.
    let time = one
        .split(r#"<td class="trace-time"><time datetime=""#)
        .nth(1)
        .unwrap();
    let (full, shown) = time.split_once("\">").unwrap();
    assert_eq!(full.len(), 24, "{full}");
    assert_eq!(&shown[..12], &full[11..23], "{one}");
    assert!(!html.contains("style="), "no inline styles");
}

#[tokio::test]
async fn the_header_has_both_tabs_with_counts_and_the_receivers_line() {
    let session = Session::new().await;
    let html = session.get("/telemetry/traces").await;
    assert!(
        html.contains(r#"<nav class="crumb" aria-label="Breadcrumb">~/telemetry/traces</nav>"#),
        "{html}"
    );
    let traces = html
        .find(r#"<a role="tab" href="/telemetry/traces" aria-selected="true" aria-current="page">Traces <span class="tab-count">4</span></a>"#)
        .expect(&html);
    let logs = html
        .find(r#"<a role="tab" href="/telemetry/logs" aria-selected="false">Logs <span class="tab-count">2</span></a>"#)
        .expect(&html);
    assert!(traces < logs, "Traces is the first tab");
    // The receiver stores what it gets while the log store is open; no gRPC port.
    assert!(
        html.contains(r#"<span class="dot receiver-on" aria-hidden="true"></span>otlp receiver · http://host.containers.internal</span>"#),
        "{html}"
    );
    assert!(!html.contains("grpc"), "{html}");
    // Stored spans and log records per second over the last minute.
    let rates = html
        .split("<span>")
        .find_map(|s| {
            s.split("</span>")
                .next()
                .filter(|s| s.contains(" spans/s · "))
        })
        .expect(&html);
    assert!(rates.ends_with(" logs/s"), "{rates}");
    assert!(html.contains("retention 7d, spans 3d · "), "{html}");

    // A stack and range picked go with the tab links, both ways.
    let html = session
        .get("/telemetry/traces?stack=notes&since=1h&status=error")
        .await;
    assert!(
        html.contains(r#"<a role="tab" href="/telemetry/logs?stack=notes&#38;since=1h" aria-selected="false">Logs"#),
        "{html}"
    );
    let html = session.get("/telemetry/logs?stack=notes&since=6h").await;
    assert!(
        html.contains(r#"<a role="tab" href="/telemetry/traces?stack=notes&#38;since=6h" aria-selected="false">Traces <span class="tab-count">4</span></a>"#),
        "{html}"
    );
    // The Logs tab's 7d and hoster's own records are not the Traces tab's: left out of the
    // link, and the count is of what the link opens.
    let tab = r#"<a role="tab" href="/telemetry/traces" aria-selected="false">Traces <span class="tab-count">4</span></a>"#;
    let opened = r#"Traces <span class="tab-count">4</span>"#;
    assert!(
        session
            .get("/telemetry/traces")
            .await
            .contains(&format!(r#"aria-current="page">{opened}"#))
    );
    // Not 5: trace 9, two hours ago, is in the 7 days but not in the 15 minutes opened.
    let html = session.get("/telemetry/logs?since=7d").await;
    assert!(html.contains(tab), "{html}");
    // Not 0: the link opens every stack's traces.
    let html = session.get("/telemetry/logs?stack=_hoster").await;
    assert!(html.contains(tab), "{html}");
    let html = session.get("/telemetry/logs?stack=_hoster&since=7d").await;
    assert!(html.contains(tab), "{html}");
}

#[tokio::test]
async fn the_header_says_when_the_receiver_stores_nothing() {
    let mut app = TestApp::new().await;
    let browser = app.sign_in().await;
    app.app.logs = LogReader::default();
    let html = body(app.get("/telemetry/traces", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<span class="dot receiver-off" aria-hidden="true"></span>otlp receiver · not storing: the log store is unavailable</span>"#),
        "{html}"
    );
    assert!(html.contains("— spans/s · — logs/s"), "{html}");
    assert!(
        html.contains(r#"<p class="card-body card-error">"#),
        "{html}"
    );
}

#[tokio::test]
async fn the_list_sorts_by_newest_slowest_or_errors_first() {
    let session = Session::new().await;
    let html = session.get("/telemetry/traces?sort=slowest").await;
    assert_eq!(names(&html), ["nightly", "GET /b", "GET /a", "GET /c"]);
    assert!(html.contains("· 4 of 4 · slowest first</span>"), "{html}");
    assert!(
        html.contains(r#"<option value="slowest" selected>slowest</option>"#),
        "{html}"
    );
    let html = session.get("/telemetry/traces?sort=errors").await;
    assert_eq!(names(&html), ["GET /a", "GET /c", "GET /b", "nightly"]);
    assert!(
        html.contains(r#"<option value="errors" selected>errors first</option>"#),
        "{html}"
    );
    // The sort form keeps every other option, and edits the sort.
    let html = session
        .get("/telemetry/traces?sort=errors&status=ok&since=1h&trace=01010101010101010101010101010101")
        .await;
    let form = html.split(r#"<form class="trace-sort""#).nth(1).unwrap();
    let form = form.split("</form>").next().unwrap();
    assert!(
        form.contains(r#"<input type="hidden" name="since" value="1h">"#),
        "{form}"
    );
    assert!(
        form.contains(r#"<input type="hidden" name="status" value="ok">"#),
        "{form}"
    );
    assert!(
        form.contains(
            r#"<input type="hidden" name="trace" value="01010101010101010101010101010101">"#
        ),
        "{form}"
    );
    assert!(!form.contains(r#"name="sort" value"#), "{form}");
    // An order that is not one: the newest, and why.
    let html = session.get("/telemetry/traces?sort=fastest").await;
    assert_eq!(names(&html), ["GET /a", "GET /b", "GET /c", "nightly"]);
    assert!(
        html.contains("`fastest` is not an order (newest, slowest or errors); showing the newest."),
        "{html}"
    );
}

#[tokio::test]
async fn filters_narrow_the_list_and_the_heading_says_of_how_many() {
    let session = Session::new().await;
    let html = session.get("/telemetry/traces?status=error").await;
    assert_eq!(names(&html), ["GET /a", "GET /c"]);
    assert!(
        html.contains("· 2 of 2 matching · 4 in range · newest first</span>"),
        "{html}"
    );
    assert!(
        html.contains(r#"<a class="log-chip" href="/telemetry/traces?status=error" aria-current="page"><span class="dot trace-error" aria-hidden="true"></span>error</a>"#),
        "{html}"
    );
    // The tab's count stays the range's.
    assert!(
        html.contains(r#"Traces <span class="tab-count">4</span>"#),
        "{html}"
    );
    assert_eq!(
        names(&session.get("/telemetry/traces?status=ok").await),
        ["GET /b", "nightly"]
    );
    assert_eq!(
        names(&session.get("/telemetry/traces?slower=100ms").await),
        ["GET /b", "nightly"]
    );
    // Grep on span names: the root's or a stored span's, ignoring case.
    assert_eq!(
        names(&session.get("/telemetry/traces?q=LOOKUP").await),
        ["GET /a"]
    );
    assert_eq!(
        names(&session.get("/telemetry/traces?q=%2Fc").await),
        ["GET /c"]
    );
    assert_eq!(
        names(&session.get("/telemetry/traces?stack=blog").await),
        ["GET /c"]
    );
    // A longer range reaches trace 9.
    assert_eq!(
        names(&session.get("/telemetry/traces?since=6h").await),
        ["GET /a", "GET /b", "GET /c", "nightly", "GET /old"]
    );

    // Nothing matches: says so and links the list without the filters, the rest kept.
    let html = session
        .get("/telemetry/traces?stack=notes&status=error&slower=1s")
        .await;
    assert!(
        html.contains("No traces match these filters in this range."),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/telemetry/traces?stack=notes">clear filters</a>"#),
        "{html}"
    );
    // A range with nothing at all.
    let html = session.get("/telemetry/traces?stack=gone").await;
    assert!(html.contains("No traces in this range."), "{html}");
    assert!(
        html.contains(r#"<option value="gone" selected>gone</option>"#),
        "{html}"
    );

    // Options that do not parse are left out, and the flash says why.
    let html = session
        .get("/telemetry/traces?status=broken&slower=soon&since=7d")
        .await;
    assert_eq!(names(&html), ["GET /a", "GET /b", "GET /c", "nightly"]);
    for problem in [
        "`7d` is not a range of the trace list (5m, 15m, 1h, 6h or 24h); showing the last 15m.",
        "`broken` is not a status (ok or error); showing every trace.",
        "`soon` is not a duration such as 500ms; showing traces of any duration.",
    ] {
        assert!(html.contains(problem), "{problem}: {html}");
    }
}

#[tokio::test]
async fn the_toolbar_offers_the_ranges_stacks_and_keeps_the_options() {
    let session = Session::new().await;
    let html = session
        .get("/telemetry/traces?status=error&since=1h&q=GET&page=0")
        .await;
    let presets: Vec<&str> = html
        .split(r#"<a class="log-preset" href=""#)
        .skip(1)
        .map(|rest| rest.split("</a>").next().unwrap())
        .collect();
    assert_eq!(
        presets,
        [
            r#"/telemetry/traces?since=5m&#38;q=GET&#38;status=error">5m"#,
            r#"/telemetry/traces?since=15m&#38;q=GET&#38;status=error">15m"#,
            r#"/telemetry/traces?since=1h&#38;q=GET&#38;status=error" aria-current="true">1h"#,
            r#"/telemetry/traces?since=6h&#38;q=GET&#38;status=error">6h"#,
            r#"/telemetry/traces?since=24h&#38;q=GET&#38;status=error">24h"#,
        ],
        "no 7d"
    );
    // Every stack, then the state's; hoster's own records name no trace, so no `hoster`.
    let options: Vec<&str> = html
        .split("<option ")
        .skip(1)
        .map(|rest| rest.split("</option>").next().unwrap())
        .take_while(|option| !option.starts_with(r#"value="newest""#))
        .collect();
    assert_eq!(
        options,
        [r#"value="" selected>all stacks"#, r#"value="notes">notes"#],
        "{html}"
    );
    // The form edits the stack, grep and duration, and keeps the rest.
    let form = html.split(r#"<form class="log-options""#).nth(1).unwrap();
    let form = form.split("</form>").next().unwrap();
    assert!(
        form.contains(r#"<input type="hidden" name="since" value="1h">"#),
        "{form}"
    );
    assert!(
        form.contains(r#"<input type="hidden" name="status" value="error">"#),
        "{form}"
    );
    assert!(
        form.contains(r#"<input type="search" name="q" value="GET""#),
        "{form}"
    );
    assert!(!form.contains(r#"name="page""#), "{form}");
}

#[tokio::test]
async fn a_trace_selected_by_its_id_is_marked_and_shown_below_the_list() {
    let session = Session::new().await;
    let list = session
        .get("/telemetry/traces?status=error&sort=slowest")
        .await;
    let href = select_href(&list, "GET /a");
    assert_eq!(
        href,
        format!(
            "/telemetry/traces?status=error&sort=slowest&trace={}",
            trace(1)
        )
    );
    let html = session.get(&href).await;
    let one = row(&html, "GET /a");
    assert!(one.starts_with(r#"<tr class="trace-selected">"#), "{one}");
    assert!(one.contains(r#" aria-current="true">GET /a</a>"#), "{one}");
    assert!(!row(&html, "GET /c").contains("trace-selected"));
    // The trace below the list, after it.
    let list_at = html.find(r#"class="card trace-list""#).unwrap();
    let trace_at = html
        .find(r#"<section class="card trace-waterfall""#)
        .expect(&html);
    assert!(list_at < trace_at);
    let shown = &html[trace_at..];
    assert!(
        shown.contains(r#"<span class="trace-kicker">TRACE · notes</span>"#),
        "{shown}"
    );
    assert!(
        shown.contains(r#"<h2 id="trace-title">GET /a</h2>"#),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(r#"<code class="trace-id">{}</code>"#, trace(1))),
        "{shown}"
    );
    assert!(shown.contains("<dt>SPANS</dt><dd>4</dd>"), "{shown}");

    // Links keep the selection; a filter or range drops the page, not the trace.
    let html = session
        .get(&format!("/telemetry/traces?page=0&trace={}", trace(1)))
        .await;
    assert!(
        html.contains(&format!(
            r#"href="/telemetry/traces?since=1h&#38;trace={}">1h</a>"#,
            trace(1)
        )),
        "{html}"
    );
    // Selecting another trace keeps the page.
    assert_eq!(
        select_href(&html, "GET /b"),
        format!("/telemetry/traces?page=0&trace={}", trace(2))
    );

    // A trace outside the range is shown all the same.
    let html = session
        .get(&format!("/telemetry/traces?trace={}", trace(9)))
        .await;
    assert!(!names(&html).contains(&"GET /old"));
    assert!(
        html.contains(r#"<h2 id="trace-title">GET /old</h2>"#),
        "{html}"
    );
}

/// The waterfall's rows of `html`: each one's markup.
fn spans(html: &str) -> Vec<&str> {
    html.split(r#"<li class="wf-row "#)
        .skip(1)
        .map(|rest| rest.split("</li>").next().unwrap())
        .collect()
}

#[tokio::test]
async fn the_selected_trace_is_drawn_as_a_waterfall_below_the_list() {
    let session = Session::new().await;
    let html = session
        .get(&format!("/telemetry/traces?page=0&trace={}", trace(1)))
        .await;
    let at = html
        .find(r#"<section class="card trace-waterfall""#)
        .expect(&html);
    assert!(html.find(r#"class="card trace-list""#).unwrap() < at);
    let shown = &html[at..];
    assert!(!shown.contains("style="), "no inline styles: {shown}");
    // The header's figures, errors in red.
    for figure in [
        "<dt>SPANS</dt><dd>4</dd>",
        "<dt>DEPTH</dt><dd>4</dd>",
        r#"<dt>ERRORS</dt><dd class="trace-error">1</dd>"#,
    ] {
        assert!(shown.contains(figure), "{figure}: {shown}");
    }
    assert!(
        shown.contains("<dt>DURATION</dt><dd>50.0 ms</dd>"),
        "{shown}"
    );
    assert!(shown.contains(" UTC</time></p>"), "{shown}");
    // The legend: the proxy first, in its colour.
    assert!(
        shown.contains(r#"<li><span class="dot svc-hoster" aria-hidden="true"></span>hoster <span class="wf-count">2 spans</span></li>"#),
        "{shown}"
    );
    assert!(
        shown.contains(r#"<span class="wf-count">1 span</span></li>"#),
        "{shown}"
    );
    // A minimap line per span.
    let minimap = shown.split(r#"<svg class="wf-minimap""#).nth(1).unwrap();
    let minimap = minimap.split("</svg>").next().unwrap();
    assert_eq!(minimap.matches("<rect ").count(), 4, "{minimap}");
    // The rows in depth-first order, each a link to its span that keeps the list's options.
    let rows = spans(shown);
    let names: Vec<&str> = rows
        .iter()
        .map(|row| {
            let rest = row.split(r#"<span class="wf-label">"#).nth(1).unwrap();
            rest.split("</span>").next().unwrap()
        })
        .collect();
    assert_eq!(names, ["GET /a", "upstream web", "GET /a", "lookup"]);
    assert!(
        rows[0].starts_with(r#"svc-hoster wf-critical">"#),
        "{}",
        rows[0]
    );
    assert!(rows[0].contains(r#"<div class="wf-name wf-depth-0">"#));
    assert!(rows[3].contains(r#"<div class="wf-name wf-depth-3">"#));
    assert!(
        rows[3].contains(r#"<span class="wf-err" role="img" aria-label="error">!</span>"#),
        "{}",
        rows[3]
    );
    assert!(rows[3].contains(r#"<rect class="wf-ring""#), "{}", rows[3]);
    assert!(!rows[2].contains("wf-err"), "{}", rows[2]);
    // It opens on the span in error, whose link closes its details.
    assert!(rows[3].contains(" wf-selected"), "{}", rows[3]);
    let close = format!(
        "/telemetry/traces?page=0&#38;trace={}&#38;span=none",
        trace(1)
    );
    assert!(
        rows[3].contains(&format!(
            r#"<a class="wf-pick" href="{close}" aria-current="true">"#
        )),
        "{}",
        rows[3]
    );
    assert_eq!(shown.matches("aria-current").count(), 1, "{shown}");
    let href = format!(
        "/telemetry/traces?page=0&#38;trace={}&#38;span={}",
        trace(1),
        sid(1, 2)
    );
    assert!(
        rows[2].contains(&format!(r#"<a class="wf-pick" href="{href}">"#)),
        "{}",
        rows[2]
    );

    // Following another marks that span; selecting another trace drops it.
    let html = session.get(&href.replace("&#38;", "&")).await;
    let rows = spans(&html);
    assert!(rows[2].contains(" wf-selected"), "{}", rows[2]);
    assert!(rows[2].contains(r#" aria-current="true">"#), "{}", rows[2]);
    assert!(!rows[3].contains(" wf-selected"), "{}", rows[3]);
    assert_eq!(
        select_href(&html, "GET /b"),
        format!("/telemetry/traces?page=0&trace={}", trace(2))
    );
    // Closing selects none.
    let html = session.get(&close.replace("&#38;", "&")).await;
    assert!(!html.contains("wf-selected"), "{html}");
    assert!(!html.contains("wf-details"), "{html}");
}

/// The mockup's `t1` as trace 6 through the app, a minute ago: the proxy's spans of a
/// `POST /api/documents.info` answered 500, and the app's spans under them down to `s9`, the
/// statement that timed out, with its attributes (one of them markup), resource, exception
/// event and two log records; a record of another span besides.
async fn t1_app() -> Session {
    let app = TestApp::new().await;
    let mut request = request(6, 60, "notes", "/api/documents.info", 500, 1_912);
    request.method = "POST".into();
    request.upstream_start = Some(MicroTimestamp::from(request.time) + Duration::from_micros(600));
    let start = MicroTimestamp::from(request.time);
    let at = |ms: f64| start + Duration::from_micros((ms * 1000.0).round() as u64);
    let app_span = |k: u8, parent: u8, name: &str, at_ms: f64, ms: f64, error: bool| Span {
        trace_id: trace(6),
        span_id: sid(6, k),
        parent: Some(sid(6, parent)),
        stack: "notes".into(),
        service: "outline-app".into(),
        name: name.into(),
        kind: SpanKind::Internal,
        start: at(at_ms),
        duration: Duration::from_micros((ms * 1000.0).round() as u64),
        status: if error {
            SpanStatus::Error
        } else {
            SpanStatus::Unset
        },
        status_message: error.then(|| "QueryCanceledError".to_owned()),
        attributes: None,
        resource: None,
        events: None,
        received: Timestamp::now(),
    };
    let mut s9 = app_span(9, 6, "SELECT revisions", 51.7, 1800.4, true);
    s9.kind = SpanKind::Client;
    s9.status_message = Some("canceling statement due to statement timeout".into());
    s9.attributes = Some(
        serde_json::json!({
            "db.system": "postgresql",
            "db.query.text": "SELECT r.* FROM revisions r WHERE r.document_id = $1 ORDER BY r.created_at DESC LIMIT 100 OFFSET 0 -- a long statement that wraps",
            "error.type": "QueryCanceledError",
            "user.note": "<script>alert(1)</script>",
        })
        .to_string(),
    );
    s9.resource = Some(
        serde_json::json!({
            "service.name": "outline-app",
            "service.version": "0.81.1",
            "telemetry.sdk.language": "nodejs",
            "service.namespace": "notes",
            "hoster.generation": "4",
        })
        .to_string(),
    );
    s9.events = Some(
        serde_json::json!([{
            "name": "exception",
            "time_us": at(1852.1).as_micros(),
            "attributes": {
                "exception.type": "SequelizeDatabaseError",
                "exception.stacktrace": "at Query.run (query.js:87:18)\nat <anonymous>",
            },
        }])
        .to_string(),
    );
    let log = |ms: f64, span: u8, level, message: &str| LogRecord {
        time: Timestamp::from_millis(at(ms).as_micros() / 1000),
        level: Some(level),
        trace_id: Some(trace(6)),
        span_id: Some(sid(6, span)),
        stream: Stream::Otlp,
        ..record(0, message)
    };
    app.logs
        .append(&LogBatch {
            records: vec![
                log(30.0, 6, crate::store::Level::Info, "loading document"),
                log(1852.0, 9, crate::store::Level::Error, "query <b>failed</b>"),
                log(1853.0, 9, crate::store::Level::Warn, "rolling back"),
            ],
            requests: vec![request],
            spans: vec![
                // The app's entry span, which "collapse to services" collapses.
                Span {
                    kind: SpanKind::Server,
                    ..app_span(2, 1, "POST /api/documents.info", 3.8, 1905.2, true)
                },
                app_span(3, 2, "authenticate", 5.1, 17.6, false),
                app_span(4, 3, "GET", 6.9, 1.8, false),
                app_span(5, 3, "SELECT users", 10.4, 9.2, false),
                app_span(6, 2, "documents.info", 23.9, 1880.3, true),
                app_span(12, 6, "GET", 24.6, 0.9, false),
                app_span(7, 6, "SELECT documents", 26.2, 14.1, false),
                app_span(8, 6, "SELECT collections", 41.0, 8.3, false),
                s9,
                app_span(10, 6, "ROLLBACK", 1853.6, 2.7, false),
            ],
            ..LogBatch::default()
        })
        .await
        .unwrap();
    Session {
        cookie: app.sign_in().await.cookie,
        app,
    }
}

/// The details under the selected row of `html`.
fn details(html: &str) -> &str {
    let at = html
        .find(r#"<section class="wf-details""#)
        .unwrap_or_else(|| panic!("no details: {html}"));
    let rest = &html[at..];
    &rest[..rest.find("</section>").unwrap()]
}

#[tokio::test]
async fn t1_opens_on_the_failing_statement_with_every_section_of_its_details() {
    let session = t1_app().await;
    let html = session
        .get(&format!("/telemetry/traces?trace={}", trace(6)))
        .await;
    let waterfall = &html[html
        .find(r#"<section class="card trace-waterfall""#)
        .unwrap()..];
    assert!(
        !waterfall.contains("style="),
        "no inline styles: {waterfall}"
    );
    // s9 is selected, and its details sit inside its row, after its bar.
    let rows = spans(waterfall);
    let selected: Vec<&&str> = rows.iter().filter(|r| r.contains(" wf-selected")).collect();
    assert_eq!(selected.len(), 1, "{waterfall}");
    assert!(
        selected[0].contains(r#"<span class="wf-label">SELECT revisions</span>"#),
        "{}",
        selected[0]
    );
    assert!(selected[0].contains(r#"</div><section class="wf-details""#));
    let shown = details(waterfall);

    let close = format!("/telemetry/traces?trace={}&#38;span=none", trace(6));
    for part in [
        // The title line.
        r#"<span class="wf-swatch" aria-hidden="true"></span>"#,
        r#"<h3 id="wf-details-title">SELECT revisions</h3>"#,
        r#"<span class="wf-details-service">outline-app</span>"#,
        r#"<span class="pill pill-neutral">client</span>"#,
        r#"<span class="pill pill-fail">error</span>"#,
        // Timing and ids.
        "<div><dt>START</dt><dd>+51.7 ms</dd></div>",
        r#"<div><dt>DURATION</dt><dd>1.80 s <span class="wf-share">· 94.2% of trace</span></dd></div>"#,
        "<div><dt>SELF TIME</dt><dd>1.80 s</dd></div>",
        // The status message.
        r#"<p class="wf-status-message">status: ERROR · canceling statement due to statement timeout</p>"#,
        // Attributes, error.type in red.
        r#"<div><dt>db.system</dt><dd>postgresql</dd></div>"#,
        r#"<div class="wf-alert"><dt>error.type</dt><dd>QueryCanceledError</dd></div>"#,
        // The resource.
        r#"<span class="wf-chip"><span class="wf-chip-key">service.name=</span>outline-app</span>"#,
        r#"<span class="wf-chip"><span class="wf-chip-key">service.namespace=</span>notes</span>"#,
        r#"<span class="wf-chip"><span class="wf-chip-key">hoster.generation=</span>4</span>"#,
        // The exception, its stack trace as it is.
        r#"<div class="wf-event wf-exception">"#,
        r#"<span class="wf-event-name">exception</span><span class="wf-event-at">@ 1.85 s (+1.80 s in span)</span>"#,
        "<div><dt>exception.type</dt><dd>SequelizeDatabaseError</dd></div>",
        "<pre class=\"wf-stacktrace\">at Query.run (query.js:87:18)\nat &#60;anonymous&#62;</pre>",
    ] {
        assert!(shown.contains(part), "{part}: {shown}");
    }
    assert!(
        shown.contains(&format!(
            r#"<a class="wf-close" href="{close}" aria-label="Close span details""#
        )),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(
            "<div><dt>SPAN ID</dt><dd>{}</dd></div>",
            sid(6, 9)
        )),
        "{shown}"
    );
    let parent = format!(
        r#"<a href="/telemetry/traces?trace={}&#38;span={}">{}</a>"#,
        trace(6),
        sid(6, 6),
        sid(6, 6)
    );
    assert!(shown.contains(&parent), "{parent}: {shown}");
    // Every value escaped.
    assert!(!shown.contains("<script>"), "{shown}");
    assert!(
        shown.contains("<dd>&#60;script&#62;alert(1)&#60;/script&#62;</dd>"),
        "{shown}"
    );
    // The span's own records, oldest first, each a link to the Logs tab; not the other span's.
    let logs: Vec<&str> = shown
        .split(r#"<a class="wf-log "#)
        .skip(1)
        .map(|rest| rest.split("</a>").next().unwrap())
        .collect();
    assert_eq!(logs.len(), 2, "{shown}");
    assert!(
        logs[0].starts_with(&format!(
            r#"log-error" href="/telemetry/logs?trace={}&#38;after="#,
            trace(6)
        )),
        "{}",
        logs[0]
    );
    assert!(
        logs[0].ends_with(r#"<span class="log-lvl">error</span><span class="log-msg">query &#60;b&#62;failed&#60;/b&#62;</span>"#),
        "{}",
        logs[0]
    );
    assert!(logs[1].contains("rolling back"), "{}", logs[1]);
    assert!(!shown.contains("loading document"), "{shown}");

    // The Logs tab link opens on the record.
    let href = logs[0]
        .split(r#"href=""#)
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .replace("&#38;", "&");
    let page = session.get(&href).await;
    let first = page.find("query &#60;b&#62;failed").expect(&page);
    assert!(!page[..first].contains("loading document"), "{page}");

    // Closing selects none, and the trace stays open without details.
    let html = session.get(&close.replace("&#38;", "&")).await;
    assert!(html.contains(r#"<section class="card trace-waterfall""#));
    assert!(!html.contains("wf-details"), "{html}");
}

#[tokio::test]
async fn the_waterfall_links_the_traces_records_with_their_count() {
    let session = t1_app().await;
    let html = session
        .get(&format!("/telemetry/traces?trace={}", trace(6)))
        .await;
    let link = format!(
        r#"<a class="wf-action" href="/telemetry/logs?trace={}">logs in trace (3)<span aria-hidden="true">&nbsp;→</span></a>"#,
        trace(6)
    );
    // In the controls, after the collapse links.
    let controls = html
        .split(r#"<div class="wf-controls">"#)
        .nth(1)
        .and_then(|rest| rest.split("</div>").next())
        .expect(&html);
    let at = controls.find(&link).expect(controls);
    assert!(
        controls.find("collapse to services").unwrap() < at,
        "{controls}"
    );
    // It lists the trace's records of every span, every level.
    let page = session
        .get(&format!("/telemetry/logs?trace={}", trace(6)))
        .await;
    for message in [
        "loading document",
        "query &#60;b&#62;failed&#60;/b&#62;",
        "rolling back",
    ] {
        assert!(page.contains(message), "{message}: {page}");
    }
    assert!(
        page.contains(r#"<a class="log-chip" href="/telemetry/logs?trace="#)
            && page.contains(
                r#"aria-current="page"><span class="dot lvl-all" aria-hidden="true"></span>all</a>"#
            ),
        "{page}"
    );

    // A trace without records counts none.
    let session = Session::new().await;
    let html = session
        .get(&format!("/telemetry/traces?trace={}", trace(1)))
        .await;
    assert!(
        html.contains(&format!(
            r#"href="/telemetry/logs?trace={}">logs in trace (0)<span"#,
            trace(1)
        )),
        "{html}"
    );
}

/// The row of `html`'s waterfall whose span is `name`, the first of that name.
fn span_row<'a>(html: &'a str, name: &str) -> &'a str {
    let label = format!(r#"<span class="wf-label">{name}</span>"#);
    spans(html)
        .into_iter()
        .find(|row| row.contains(&label))
        .unwrap_or_else(|| panic!("no row {name}: {html}"))
}

#[tokio::test]
async fn spans_collapse_and_expand_by_links_kept_in_the_url() {
    let session = t1_app().await;
    let html = session
        .get(&format!("/telemetry/traces?trace={}", trace(6)))
        .await;
    let waterfall = &html[html
        .find(r#"<section class="card trace-waterfall""#)
        .unwrap()..];
    assert!(
        !waterfall.contains("style="),
        "no inline styles: {waterfall}"
    );
    assert_eq!(spans(waterfall).len(), 12);
    // A span with rows under it has a toggle before its link, named for what it does.
    let collapse = format!(
        "/telemetry/traces?trace={}&#38;collapsed={}",
        trace(6),
        sid(6, 6)
    );
    let row = span_row(waterfall, "documents.info");
    assert!(
        row.contains(&format!(
            r#"<a class="wf-toggle" href="{collapse}" aria-expanded="true" aria-label="Collapse documents.info" title="Collapse documents.info"><svg class="icon""#
        )),
        "{row}"
    );
    assert!(row.contains(r##"#disclosure"></use></svg></a>"##), "{row}");
    assert!(!row.contains("wf-hidden"), "{row}");
    // A leaf has a spacer instead.
    let leaf = span_row(waterfall, "ROLLBACK");
    assert!(!leaf.contains("wf-toggle"), "{leaf}");
    assert!(
        leaf.contains(r#"<span class="wf-leaf" aria-hidden="true"></span>"#),
        "{leaf}"
    );
    // Nothing is collapsed: "expand all" is off.
    assert!(
        waterfall.contains(r#"<span class="wf-action wf-action-off">expand all</span>"#),
        "{waterfall}"
    );

    // Following the toggle collapses the span: its five rows are hidden and it says so.
    let html = session.get(&collapse.replace("&#38;", "&")).await;
    assert_eq!(spans(&html).len(), 7, "{html}");
    let row = span_row(&html, "documents.info");
    assert!(
        row.contains(&format!(
            r#"<a class="wf-toggle" href="/telemetry/traces?trace={}" aria-expanded="false" aria-label="Expand documents.info" title="Expand documents.info">"#,
            trace(6)
        )),
        "{row}"
    );
    assert!(
        row.contains(r#"<span class="wf-hidden" aria-hidden="true">+5</span>"#),
        "{row}"
    );
    assert!(row.contains("; 5 spans collapsed under it"), "{row}");
    for hidden in ["SELECT revisions", "ROLLBACK", "SELECT documents"] {
        assert!(
            !html.contains(&format!(r#"<span class="wf-label">{hidden}</span>"#)),
            "{hidden}: {html}"
        );
    }
    // The trace opens on s9, which is hidden now: no details, and the trace stays open.
    assert!(!html.contains("wf-details"), "{html}");
    assert!(!html.contains("wf-selected"), "{html}");
    // Collapsing another keeps this one; the set is written in row order.
    let authenticate = span_row(&html, "authenticate");
    assert!(
        authenticate.contains(&format!(
            r#"href="/telemetry/traces?trace={}&#38;collapsed={}%2C{}" aria-expanded="true""#,
            trace(6),
            sid(6, 3),
            sid(6, 6)
        )),
        "{authenticate}"
    );
    // "expand all" clears it, and the details are back.
    let expand = format!(
        r#"<a class="wf-action" href="/telemetry/traces?trace={}">expand all</a>"#,
        trace(6)
    );
    assert!(html.contains(&expand), "{html}");
    let html = session
        .get(&format!("/telemetry/traces?trace={}", trace(6)))
        .await;
    assert!(html.contains("wf-details"), "{html}");

    // Ids the trace does not have, or that hide nothing, are ignored without an error.
    let html = session
        .get(&format!(
            "/telemetry/traces?trace={}&collapsed=zz,{},{}",
            trace(6),
            sid(6, 10),
            sid(7, 6)
        ))
        .await;
    assert_eq!(spans(&html).len(), 12, "{html}");
    assert!(!html.contains("card-error"), "{html}");
}

#[tokio::test]
async fn collapse_to_services_collapses_the_apps_entry_span() {
    let session = t1_app().await;
    let html = session
        .get(&format!("/telemetry/traces?trace={}&span=none", trace(6)))
        .await;
    let services = format!(
        "/telemetry/traces?trace={}&#38;span=none&#38;collapsed={}",
        trace(6),
        sid(6, 2)
    );
    assert!(
        html.contains(&format!(
            r#"<a class="wf-action" href="{services}">collapse to services</a>"#
        )),
        "{html}"
    );
    let html = session.get(&services.replace("&#38;", "&")).await;
    let names: Vec<&str> = spans(&html)
        .into_iter()
        .map(|row| {
            let rest = row.split(r#"<span class="wf-label">"#).nth(1).unwrap();
            rest.split("</span>").next().unwrap()
        })
        .collect();
    assert_eq!(
        names,
        [
            "POST /api/documents.info",
            "upstream web",
            "POST /api/documents.info"
        ]
    );
    let entry = spans(&html)[2];
    assert!(
        entry.contains(r#"<span class="wf-hidden" aria-hidden="true">+9</span>"#),
        "{entry}"
    );
    assert!(
        entry.contains(r#"aria-expanded="false" aria-label="Expand POST /api/documents.info""#),
        "{entry}"
    );
    // That is what is collapsed now, so the link is off.
    assert!(
        html.contains(r#"<span class="wf-action wf-action-off">collapse to services</span>"#),
        "{html}"
    );
    // Selecting another trace drops the collapsed set, the span with it.
    assert_eq!(
        TraceParams::from(vec![
            ("trace".to_owned(), trace(6).to_string()),
            ("collapsed".to_owned(), sid(6, 2).to_string()),
        ])
        .select(trace(1)),
        format!("/telemetry/traces?trace={}", trace(1))
    );
}

#[tokio::test]
async fn the_critical_path_switch_dims_by_css_with_crit_on_as_its_fallback() {
    let session = t1_app().await;
    let html = session
        .get(&format!("/telemetry/traces?trace={}", trace(6)))
        .await;
    assert!(
        html.contains(r#"<section class="card trace-waterfall" aria-labelledby="trace-title">"#),
        "{html}"
    );
    // The rows on the path are marked, and say so.
    let critical: Vec<&str> = spans(&html)
        .into_iter()
        .filter(|row| row.contains(r#" wf-critical">"#))
        .map(|row| {
            assert!(row.contains(", on the critical path"), "{row}");
            let rest = row.split(r#"<span class="wf-label">"#).nth(1).unwrap();
            rest.split("</span>").next().unwrap()
        })
        .collect();
    assert_eq!(
        critical,
        [
            "POST /api/documents.info",
            "upstream web",
            "POST /api/documents.info",
            "documents.info",
            "ROLLBACK"
        ]
    );
    // The switch: a checkbox, and the link that stands in for it without `:has()`.
    assert!(
        html.contains(r#"<label class="switch wf-crit-switch"><input type="checkbox" role="switch" class="wf-crit-input"><span class="switch-track" aria-hidden="true"></span><span class="visually-hidden">highlight </span>critical path</label>"#),
        "{html}"
    );
    let on = format!("/telemetry/traces?trace={}&#38;crit=on", trace(6));
    assert!(
        html.contains(&format!(
            r#"<a class="switch wf-crit-link" role="switch" aria-checked="false" href="{on}">"#
        )),
        "{html}"
    );

    // With crit=on the server dims the rows and the switch is on.
    let html = session.get(&on.replace("&#38;", "&")).await;
    assert!(
        html.contains(
            r#"<section class="card trace-waterfall wf-crit-on" aria-labelledby="trace-title">"#
        ),
        "{html}"
    );
    assert!(html.contains(r#"class="wf-crit-input" checked>"#), "{html}");
    assert!(
        html.contains(&format!(
            r#"<a class="switch wf-crit-link" role="switch" aria-checked="true" href="/telemetry/traces?trace={}">"#,
            trace(6)
        )),
        "{html}"
    );
    // It is kept by the other links, selecting a trace included.
    let row = span_row(&html, "documents.info");
    assert!(row.contains("&#38;crit=on\" aria-expanded"), "{row}");
    assert_eq!(
        select_href(&html, "POST /api/documents.info"),
        format!("/telemetry/traces?trace={}&crit=on", trace(6))
    );
}

#[tokio::test]
async fn the_proxys_root_span_has_its_attributes_and_no_events_or_logs() {
    let session = t1_app().await;
    let html = session
        .get(&format!(
            "/telemetry/traces?trace={}&span={}",
            trace(6),
            sid(6, 0)
        ))
        .await;
    let shown = details(&html);
    for part in [
        r#"<h3 id="wf-details-title">POST /api/documents.info</h3>"#,
        r#"<span class="wf-details-service">hoster</span>"#,
        r#"<span class="pill pill-neutral">server</span>"#,
        "<div><dt>START</dt><dd>+0 µs</dd></div>",
        r#"<div><dt>PARENT</dt><dd class="wf-parent">— (root)</dd></div>"#,
        r#"<p class="wf-status-message">status: ERROR · responded 500</p>"#,
        r#"<div class="wf-alert"><dt>http.response.status_code</dt><dd>500</dd></div>"#,
        "<div><dt>hoster.route.prefix</dt><dd>/</dd></div>",
        r#"<span class="wf-chip"><span class="wf-chip-key">service.name=</span>hoster</span>"#,
        r#"<p class="wf-none">no events on this span</p>"#,
        r#"<p class="wf-none">none emitted</p>"#,
    ] {
        assert!(shown.contains(part), "{part}: {shown}");
    }
    // The details are of the root only.
    assert_eq!(html.matches("wf-selected").count(), 1, "{html}");
}

#[tokio::test]
async fn a_trace_that_does_not_exist_says_it_may_have_expired() {
    let session = Session::new().await;
    let gone = trace(0x77);
    let html = session
        .get(&format!("/telemetry/traces?trace={gone}"))
        .await;
    assert!(
        html.contains(&format!(
            "No trace <code>{gone}</code> in the log store: spans are kept 3 days, so it may have expired."
        )),
        "{html}"
    );
    assert!(!html.contains("trace-waterfall"), "{html}");
    // The list is there as ever.
    assert_eq!(names(&html).len(), 4);

    let html = session.get("/telemetry/traces?trace=not-a-trace").await;
    assert!(
        html.contains("`not-a-trace` is not a trace id: 32 hex digits."),
        "{html}"
    );
    assert_eq!(names(&html).len(), 4);
}

/// The duration scatter's card.
fn scatter(html: &str) -> &str {
    let start = html
        .find(r#"<section class="card trace-scatter""#)
        .unwrap_or_else(|| panic!("no scatter: {html}"));
    let end = start + html[start..].find("</section>").unwrap();
    &html[start..end]
}

#[tokio::test]
async fn the_scatter_above_the_list_marks_the_listed_traces_with_their_rows_links() {
    let session = Session::new().await;
    let html = session
        .get(&format!("/telemetry/traces?trace={}", trace(2)))
        .await;
    let chart = scatter(&html);
    assert!(
        html.find("trace-scatter").unwrap() < html.find("trace-list").unwrap(),
        "{html}"
    );
    assert!(
        chart.contains(r#"<h2 id="scatter-title">Trace duration <span class="card-count">· root spans · last 15m · log scale</span></h2>"#),
        "{chart}"
    );
    assert!(chart.contains(r#"<ul class="chart-legend">"#), "{chart}");
    // Every listed trace is a link that selects it as its row's does; the selected one is the
    // current one, drawn last.
    for (name, took) in [
        ("GET /a", "50.0 ms"),
        ("GET /b", "300 ms"),
        ("GET /c", "10.0 ms"),
        ("nightly", "2.00 s"),
    ] {
        let href = select_href(&html, name).replace('&', "&#38;");
        assert!(
            chart.contains(&format!(
                r#"href="{href}" aria-label="Open trace {name}, {took}""#
            )),
            "{name}: {chart}"
        );
    }
    let links: Vec<&str> = chart.split(r#"<a class="scatter-link"#).skip(1).collect();
    assert_eq!(links.len(), 4, "{chart}");
    assert!(
        links[3].starts_with(r#" scatter-current" href="/telemetry/traces?trace=02020202020202020202020202020202" aria-label="Open trace GET /b, 300 ms" aria-current="true">"#),
        "{chart}"
    );
    assert_eq!(chart.matches("aria-current").count(), 1, "{chart}");
    // Every trace of the range is listed, so none is drawn behind.
    assert!(!chart.contains("scatter-dot"), "{chart}");
    assert!(
        chart.contains(r#"role="group" aria-label="Trace duration of root spans over the last 15m, log scale: 4 traces, 4 listed below; slowest error GET /a, 50.0 ms, 1m"#),
        "{chart}"
    );
}

#[tokio::test]
async fn the_scatter_follows_the_lists_filters_but_not_its_page() {
    let session = Session::new().await;
    // A page past the traces: every trace behind, none linked; errors red.
    let html = session.get("/telemetry/traces?page=1").await;
    let chart = scatter(&html);
    assert_eq!(
        chart.matches(r#"class="scatter-dot "#).count(),
        4,
        "{chart}"
    );
    assert_eq!(
        chart
            .matches(r#"class="scatter-dot scatter-error""#)
            .count(),
        2,
        "{chart}"
    );
    assert!(!chart.contains("<a "), "{chart}");
    assert!(chart.contains(r#"role="img""#), "{chart}");
    // The status filter narrows it as it narrows the list.
    let html = session.get("/telemetry/traces?status=error").await;
    let chart = scatter(&html);
    assert_eq!(chart.matches(r#"<a class="scatter-link""#).count(), 2);
    assert!(chart.contains("Open trace GET /a, 50.0 ms"), "{chart}");
    assert!(chart.contains("Open trace GET /c, 10.0 ms"), "{chart}");
    // A longer range reaches trace 9, two hours ago.
    let html = session.get("/telemetry/traces?since=6h").await;
    let chart = scatter(&html);
    assert!(
        chart.contains("· root spans · last 6h · log scale"),
        "{chart}"
    );
    assert!(chart.contains("Open trace GET /old, 5.0 ms"), "{chart}");
    assert!(
        chart.contains("<span>-6h</span><span>-4h</span><span>-2h</span><span>now</span>"),
        "{chart}"
    );
}

#[tokio::test]
async fn an_empty_range_has_an_empty_scatter_from_1_ms_to_1_s() {
    let app = TestApp::new().await;
    let session = Session {
        cookie: app.sign_in().await.cookie,
        app,
    };
    let html = session.get("/telemetry/traces").await;
    let chart = scatter(&html);
    assert!(
        chart.contains(r#"<div class="chart chart-scatter" role="img" aria-label="Trace duration of root spans over the last 15m, log scale: no traces"><div class="chart-y" aria-hidden="true"><span>1 s</span><span>100 ms</span><span>10 ms</span><span>1 ms</span></div>"#),
        "{chart}"
    );
    assert!(!chart.contains("<circle"), "{chart}");
    assert!(html.contains("No traces in this range."), "{html}");
}

#[tokio::test]
async fn the_list_pages_through_the_matching_traces() {
    let app = TestApp::new().await;
    let requests: Vec<RequestRow> = (0..52u8)
        .map(|n| request(n + 1, 10 + u64::from(n), "notes", &format!("/p{n}"), 200, 5))
        .collect();
    app.logs
        .append(&LogBatch {
            requests,
            ..LogBatch::default()
        })
        .await
        .unwrap();
    let session = Session {
        cookie: app.sign_in().await.cookie,
        app,
    };
    let html = session.get("/telemetry/traces").await;
    assert_eq!(names(&html).len(), 50);
    assert!(html.contains("· 50 of 52 · newest first</span>"), "{html}");
    assert!(
        html.contains(r#"<a href="/telemetry/traces?page=1" rel="next">next →</a>"#),
        "{html}"
    );
    let html = session.get("/telemetry/traces?page=1").await;
    assert_eq!(names(&html), ["GET /p50", "GET /p51"]);
    assert!(
        html.contains("· 51–52 of 52 · newest first</span>"),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/telemetry/traces" rel="prev">← previous</a>"#),
        "{html}"
    );
    assert!(!html.contains("next →"), "{html}");
    let html = session.get("/telemetry/traces?page=5").await;
    assert!(html.contains("No traces on this page."), "{html}");
}

#[test]
fn parameters_keep_their_first_value_in_one_order() {
    let params = TraceParams::from(
        [
            ("trace", "ab"),
            ("status", " error "),
            ("q", " GET /a "),
            ("status", "ok"),
            ("stack", ""),
            ("source", "deploy"),
            ("since", "1h"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec(),
    );
    assert_eq!(params.get("status"), Some("error"));
    assert_eq!(params.get("q"), Some(" GET /a "), "grep keeps its spaces");
    assert_eq!(params.stack(), None);
    assert_eq!(params.get("source"), None);
    assert_eq!(
        params.href(&[]),
        "/telemetry/traces?since=1h&q=+GET+%2Fa+&status=error&trace=ab"
    );
    assert_eq!(
        params.href(&[("status", None), ("page", Some("2"))]),
        "/telemetry/traces?since=1h&q=+GET+%2Fa+&page=2&trace=ab"
    );
    assert_eq!(
        params.keep(&TraceParams::FORM),
        [
            ("since", "1h".to_owned()),
            ("status", "error".to_owned()),
            ("trace", "ab".to_owned())
        ]
    );
    assert_eq!(TraceParams::default().href(&[]), "/telemetry/traces");
}

#[test]
fn selecting_a_span_keeps_the_trace_and_selecting_a_trace_drops_the_span() {
    let params = TraceParams::from(
        [
            ("span", "0101010101010101"),
            ("trace", "ab"),
            ("page", "2"),
            ("sort", "slowest"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec(),
    );
    assert_eq!(params.span(), Some(SpanId::from([1; 8])));
    assert_eq!(
        params.select_span(SpanId::from([2; 8])),
        "/telemetry/traces?sort=slowest&page=2&trace=ab&span=0202020202020202"
    );
    assert_eq!(
        params.select(trace(3)),
        format!("/telemetry/traces?sort=slowest&page=2&trace={}", trace(3))
    );
    assert_eq!(
        params.href(&[("since", Some("1h"))]),
        "/telemetry/traces?since=1h&sort=slowest&trace=ab&span=0101010101010101"
    );
    // Closing the details keeps everything else and says so, so the trace does not open on its
    // default span again; `none` selects no span.
    let closed = params.close_span();
    assert_eq!(
        closed,
        "/telemetry/traces?sort=slowest&page=2&trace=ab&span=none"
    );
    let closed = TraceParams::from(
        form_urlencoded::parse(closed.split_once('?').unwrap().1.as_bytes())
            .into_owned()
            .collect::<Vec<_>>(),
    );
    assert_eq!(closed.get("span"), Some(TraceParams::NO_SPAN));
    assert_eq!(closed.span(), None);
}

#[test]
fn the_palette_gives_the_proxy_its_colour_and_others_theirs_in_turn() {
    let mut palette = ServicePalette::default();
    palette.learn(["hoster", "web", "api"]);
    palette.learn(["worker", "web", "a", "b", "c", "d"]);
    let classes: Vec<&str> = ["hoster", "web", "api", "worker", "a", "b", "c", "d"]
        .into_iter()
        .map(|s| palette.class(s))
        .collect();
    assert_eq!(
        classes,
        [
            "svc-hoster",
            "svc-0",
            "svc-1",
            "svc-2",
            "svc-3",
            "svc-4",
            "svc-5",
            "svc-0"
        ]
    );
}

#[test]
fn rows_are_dated_when_any_falls_on_another_day_in_any_order() {
    let times = |list: &[&str]| list.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>();
    // Slowest first across midnight: the first and last row on one day, one between them not.
    assert!(TracesView::dated(&times(&[
        "2026-11-17T23:50:00.000Z",
        "2026-11-18T00:10:00.000Z",
        "2026-11-17T23:40:00.000Z",
    ])));
    assert!(!TracesView::dated(&times(&[
        "2026-11-17T23:50:00.000Z",
        "2026-11-17T01:10:00.000Z",
        "2026-11-17T23:40:00.000Z",
    ])));
    assert!(!TracesView::dated(&[]));
}

#[test]
fn the_collapsed_set_and_the_highlight_round_trip_and_only_the_set_is_of_a_selection() {
    let collapsed = format!("{}, {},nope", SpanId::from([2; 8]), "0303030303030303");
    let params = TraceParams::from(
        [
            ("crit", "on"),
            ("collapsed", collapsed.as_str()),
            ("trace", "ab"),
            ("page", "1"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec(),
    );
    assert_eq!(
        params.collapsed(),
        HashSet::from([SpanId::from([2; 8]), SpanId::from([3; 8])])
    );
    assert!(params.critical());
    // Selecting a span keeps both; selecting a trace keeps the highlight only.
    assert_eq!(
        params.select_span(SpanId::from([4; 8])),
        format!(
            "/telemetry/traces?page=1&trace=ab&span=0404040404040404&collapsed={}&crit=on",
            form_urlencoded::byte_serialize(collapsed.as_bytes()).collect::<String>()
        )
    );
    assert_eq!(
        params.select(trace(3)),
        format!("/telemetry/traces?page=1&trace={}&crit=on", trace(3))
    );
    assert_eq!(
        params.toggle_critical(),
        format!(
            "/telemetry/traces?page=1&trace=ab&collapsed={}",
            form_urlencoded::byte_serialize(collapsed.as_bytes()).collect::<String>()
        )
    );
    // A collapse link writes the spans with rows under them in row order, and reads back as
    // that set; none drops `collapsed`.
    let ok = SpanStatus::Unset;
    let trace = Trace::new(
        trace(3),
        vec![
            span(3, 4, Some(3), "web", "leaf", 30, 1, ok),
            span(3, 3, Some(2), "web", "middle", 40, 10, ok),
            span(3, 2, None, "web", "root", 50, 20, ok),
        ],
        3,
    );
    let href = params.collapse(&trace, &HashSet::from([sid(3, 4), sid(3, 3), sid(3, 2)]));
    assert_eq!(
        href,
        format!(
            "/telemetry/traces?page=1&trace=ab&collapsed={}%2C{}&crit=on",
            sid(3, 2),
            sid(3, 3)
        )
    );
    let read = TraceParams::from(
        form_urlencoded::parse(href.split_once('?').unwrap().1.as_bytes())
            .into_owned()
            .collect::<Vec<_>>(),
    );
    assert_eq!(read.collapsed(), HashSet::from([sid(3, 2), sid(3, 3)]));
    assert_eq!(
        params.collapse(&trace, &HashSet::new()),
        "/telemetry/traces?page=1&trace=ab&crit=on"
    );
    assert!(TraceParams::default().collapsed().is_empty());
    assert!(!TraceParams::default().critical());
}
