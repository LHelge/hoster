//! The Traces tab through the whole app, against seeded request rows and spans and the fake
//! orchestrator, whose state holds the stack `notes`; and its parameters and palette alone.

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
        one.contains(r#"<span class="trace-took">50 ms</span>"#),
        "{one}"
    );
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
    assert!(
        shown.contains(r#"<span class="card-count">4 spans · "#),
        "{shown}"
    );

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
