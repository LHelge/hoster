//! The Telemetry page through the whole app, against a seeded log store and the fake
//! orchestrator, whose state holds the stack `notes` (services `api` and `storage`).

use std::time::Duration;

use axum::http::{StatusCode, header};

use super::*;
use crate::store::{Level, LogBatch, LogRecord, Stream};
use crate::web::auth::tests::TestApp;
use crate::web::stacks::tests::body;

fn record(ago_secs: u64, stack: Option<&str>, service: &str, message: &str) -> LogRecord {
    LogRecord {
        time: Timestamp::now() - Duration::from_secs(ago_secs),
        stack: stack.map(str::to_owned),
        service: service.into(),
        generation: stack.map(|_| 1),
        stream: if service == "hoster" {
            Stream::Hoster
        } else {
            Stream::Stdout
        },
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

/// `notes` and `blog` records, hoster's own with and without a stack, and one outside the
/// default 15 minutes.
async fn seeded() -> TestApp {
    let app = TestApp::new().await;
    let mut failed = record(120, Some("notes"), "api", "request failed");
    failed.level = Some(Level::Error);
    failed.fields = Some(r#"{"user":{"id":7}}"#.into());
    let mut renewed = record(90, None, "hoster", "certificate renewed");
    renewed.level = Some(Level::Info);
    app.logs
        .append(&LogBatch {
            records: vec![
                record(3_000, Some("notes"), "api", "fifty minutes ago"),
                record(300, Some("notes"), "api", "Listening on :8080"),
                failed,
                record(100, Some("blog"), "app", "started"),
                renewed,
                record(60, Some("notes"), "hoster", "health check failed"),
            ],
            ..LogBatch::default()
        })
        .await
        .unwrap();
    app
}

/// The text of the rows, in page order.
fn messages(html: &str) -> Vec<&str> {
    html.split(r#"<span class="log-msg">"#)
        .skip(1)
        .map(|rest| {
            rest.split("</span></li>")
                .next()
                .unwrap()
                .split("</span></summary>")
                .next()
                .unwrap()
        })
        .collect()
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
async fn the_logs_tab_lists_every_stack_with_a_stack_column() {
    let app = seeded().await;
    let browser = app.sign_in().await;
    let response = app.get("/telemetry/logs", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    // The last 15 minutes by default.
    assert_eq!(
        messages(&html),
        [
            "Listening on :8080",
            "request failed",
            "started",
            "certificate renewed",
            "health check failed",
        ],
        "{html}"
    );
    assert!(
        html.contains(
            r#"<a class="log-preset" href="/telemetry/logs?since=15m" aria-current="true">15m</a>"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"class="log log-records log-stacks""#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="log-stack">blog</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="log-stack">—</span>"#),
        "{html}"
    );
    // The shell: the nav item, the crumb, the tab with its count, the header's figures.
    assert!(
        html.contains(&format!(
            r#"<a href="/telemetry" aria-current="page">{}Telemetry</a>"#,
            crate::web::Icon::Telemetry
        )),
        "{html}"
    );
    assert!(
        html.contains(r#"<nav class="crumb" aria-label="Breadcrumb">~/telemetry/logs</nav>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"aria-current="page">Logs <span class="tab-count">5</span></a>"#),
        "{html}"
    );
    assert!(html.contains(" logs/s</span>"), "{html}");
    assert!(html.contains("retention 7d · "), "{html}");
    // Not the stack tab's: no follow, no source switch, no live logs.
    assert!(!html.contains("hx-sse:connect"), "{html}");
    assert!(!html.contains("Log source"), "{html}");
    assert!(!html.contains("style="), "no inline styles");
}

#[tokio::test]
async fn the_navigation_puts_telemetry_after_stacks() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = body(app.get("/telemetry/logs", Some(&browser.cookie)).await).await;
    let at = |href: &str| html.find(&format!(r#"<li><a href="{href}""#)).unwrap();
    assert!(at("/") < at("/stacks"));
    assert!(at("/stacks") < at("/telemetry"));
    assert!(at("/telemetry") < at("/certificates"));
    assert!(at("/certificates") < at("/settings/general"));
    assert!(at("/settings/general") < at("/system"));
}

#[tokio::test]
async fn the_stack_selector_offers_every_stack_the_state_and_range_know_and_hoster() {
    let app = seeded().await;
    let browser = app.sign_in().await;
    let html = body(app.get("/telemetry/logs", Some(&browser.cookie)).await).await;
    let options: Vec<&str> = html
        .split("<option ")
        .skip(1)
        .map(|rest| rest.split("</option>").next().unwrap())
        .collect();
    assert_eq!(
        options,
        [
            r#"value="" selected>all stacks"#,
            r#"value="blog">blog"#,
            r#"value="notes">notes"#,
            r#"value="_hoster">hoster"#,
        ],
        "{html}"
    );

    // One stack: its records, hoster's own about it included, no stack column, and the
    // revision's service chips; links keep the stack.
    let html = body(
        app.get("/telemetry/logs?stack=notes", Some(&browser.cookie))
            .await,
    )
    .await;
    assert_eq!(
        messages(&html),
        [
            "Listening on :8080",
            "request failed",
            "health check failed"
        ],
        "{html}"
    );
    assert!(
        html.contains(r#"<option value="notes" selected>notes</option>"#),
        "{html}"
    );
    assert!(!html.contains("log-stacks"), "{html}");
    assert!(
        html.contains(r#"href="/telemetry/logs?stack=notes&#38;service=storage""#),
        "{html}"
    );
    assert!(
        html.contains(r#"aria-current="page">Logs <span class="tab-count">3</span></a>"#),
        "{html}"
    );
    // The form edits the stack rather than keeping it hidden.
    assert!(
        !html.contains(r#"<input type="hidden" name="stack""#),
        "{html}"
    );

    // hoster's own records that name no stack.
    let html = body(
        app.get("/telemetry/logs?stack=_hoster", Some(&browser.cookie))
            .await,
    )
    .await;
    assert_eq!(messages(&html), ["certificate renewed"], "{html}");
    assert!(
        html.contains(r#"<option value="_hoster" selected>hoster</option>"#),
        "{html}"
    );

    // A longer range lists what it reaches; a stack with no records still has its option.
    let html = body(
        app.get("/telemetry/logs?since=1h&stack=gone", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(
        html.contains(r#"<option value="gone" selected>gone</option>"#),
        "{html}"
    );
    assert!(html.contains("No records in this range"), "{html}");
}

#[tokio::test]
async fn filters_take_the_stack_tabs_parameters() {
    let app = seeded().await;
    let browser = app.sign_in().await;
    let get = |uri: &'static str| {
        let app = &app;
        let cookie = browser.cookie.clone();
        async move { body(app.get(uri, Some(&cookie)).await).await }
    };
    assert_eq!(
        messages(&get("/telemetry/logs?level=error").await),
        ["request failed"]
    );
    assert_eq!(
        messages(&get("/telemetry/logs?q=STARTED").await),
        ["<mark>started</mark>"]
    );
    assert_eq!(
        messages(&get("/telemetry/logs?field=user.id%3D7").await),
        ["request failed"]
    );
    assert_eq!(
        messages(&get("/telemetry/logs?service=app").await),
        ["started"]
    );
    assert_eq!(
        messages(&get("/telemetry/logs?since=1h&stack=notes&service=api").await),
        ["fifty minutes ago", "Listening on :8080", "request failed"]
    );
    // The stack tab's `source` is not this page's: it shows the records.
    assert_eq!(
        messages(&get("/telemetry/logs?source=deploy&stack=_hoster").await),
        ["certificate renewed"]
    );
    // A field value links this page, with the stack kept.
    let html = get("/telemetry/logs?stack=notes").await;
    assert!(
        html.contains(r#"<a href="/telemetry/logs?stack=notes&#38;field=user.id%3D7""#),
        "{html}"
    );
}

/// The links of the severity histogram's bars, oldest first, unescaped.
fn bar_links(html: &str) -> Vec<String> {
    html.split(r#"<a class="chart-bar" href=""#)
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().replace("&#38;", "&"))
        .collect()
}

#[tokio::test]
async fn the_severity_histogram_counts_the_range_by_level_and_follows_the_filters() {
    let app = seeded().await;
    let browser = app.sign_in().await;
    let get = |uri: String| {
        let app = &app;
        let cookie = browser.cookie.clone();
        async move { body(app.get(&uri, Some(&cookie)).await).await }
    };

    let html = get("/telemetry/logs".into()).await;
    assert!(
        html.contains(r#"Log records by severity <span class="card-count">· 30s buckets</span>"#),
        "{html}"
    );
    assert!(
        html.contains(
            r#"aria-label="Log records by severity for all stacks over the last 15m in 30s buckets: 5 records, error 1, warn 0, debug/info 1, no level 3; most errors "#
        ),
        "{html}"
    );
    for key in ["error 1", "warn 0", "debug/info 1", "no level 3"] {
        assert!(
            html.contains(&format!("</span>{key}</li>")),
            "{key}: {html}"
        );
    }
    // A bar per 30 seconds; each narrows the range to its bucket, the newest only from its
    // start so it keeps the newest records.
    let links = bar_links(&html);
    assert!((30..=32).contains(&links.len()), "{links:?}");
    assert!(
        links
            .iter()
            .all(|l| l.starts_with("/telemetry/logs?since="))
    );
    let (newest, older) = links.split_last().unwrap();
    assert!(!newest.contains("before="), "{newest}");
    assert!(older.iter().all(|l| l.contains("&before=")), "{older:?}");
    // The bucket of the record five minutes ago lists it alone.
    let bucket = older
        .iter()
        .find(|l| {
            let since = l.split("since=").nth(1).unwrap().split('&').next().unwrap();
            let since = humantime::parse_rfc3339(&since.replace("%3A", ":")).unwrap();
            let ago = std::time::SystemTime::now().duration_since(since).unwrap();
            (Duration::from_secs(300)..Duration::from_secs(330)).contains(&ago)
        })
        .unwrap();
    assert_eq!(messages(&get(bucket.clone()).await), ["Listening on :8080"]);

    // A level filter draws its levels only; a stack and service narrow the counts.
    let html = get("/telemetry/logs?level=error".into()).await;
    assert!(html.contains(": 1 record, error 1; most errors"), "{html}");
    assert!(!html.contains("</span>no level"), "{html}");
    let html = get("/telemetry/logs?stack=notes&service=api".into()).await;
    assert!(
        html.contains("for notes, service api over the last 15m in 30s buckets: 2 records,"),
        "{html}"
    );
    assert!(bar_links(&html)[0].starts_with("/telemetry/logs?stack=notes&service=api&since="));
    // Grep and field filters do not narrow it, and it says so.
    let html = get("/telemetry/logs?q=started".into()).await;
    assert!(
        html.contains(" · unfiltered by grep and fields</span>"),
        "{html}"
    );
    assert!(
        html.contains("not narrowed by grep or field filters: 5 records"),
        "{html}"
    );
    assert!(!html.contains("style="), "no inline styles");
}

#[test]
fn rates_have_one_decimal_below_ten() {
    let rate = |count| {
        PerSecond {
            count,
            window: Duration::from_secs(60),
        }
        .to_string()
    };
    assert_eq!(rate(0), "0.0");
    assert_eq!(rate(90), "1.5");
    assert_eq!(rate(600), "10");
    assert_eq!(rate(90_000), "1,500");
}

mod traces {
    //! The Traces tab against request rows (the proxy's spans) and stored spans of apps.

    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::store::{MicroTimestamp, RequestRow, Span, SpanId, SpanKind, SpanStatus, TraceId};

    fn trace(n: u8) -> TraceId {
        TraceId::from([n; 16])
    }

    /// A request routed `ago_secs` ago that took `millis`, the root of trace `n`.
    fn request(n: u8, ago_secs: u64, stack: &str, path: &str, millis: u64) -> RequestRow {
        let time = Timestamp::now() - Duration::from_secs(ago_secs);
        RequestRow {
            time,
            host: format!("{stack}.example.com"),
            prefix: "/".into(),
            stack: stack.into(),
            service: if stack == "notes" { "api" } else { "app" }.into(),
            method: "GET".into(),
            path: path.into(),
            status: 200,
            to_header: Some(Duration::from_millis(1)),
            total: Duration::from_millis(millis),
            bytes: 10,
            client: IpAddr::V4(Ipv4Addr::LOCALHOST),
            upstream_failed: false,
            request_id: trace(n),
            server_span: Some(SpanId::from([n; 8])),
            client_span: Some(SpanId::from([n + 100; 8])),
            upstream_start: Some(MicroTimestamp::from(time) + Duration::from_micros(100)),
        }
    }

    /// A span of `api` under trace `n`'s upstream span.
    fn app_span(n: u8, name: &str, status: SpanStatus) -> Span {
        let time = Timestamp::now() - Duration::from_secs(120);
        Span {
            trace_id: trace(n),
            span_id: SpanId::from([n + 50; 8]),
            parent: Some(SpanId::from([n + 100; 8])),
            stack: "notes".into(),
            service: "api".into(),
            name: name.into(),
            kind: SpanKind::Server,
            start: MicroTimestamp::from(time) + Duration::from_micros(200),
            duration: Duration::from_millis(800),
            status,
            status_message: None,
            attributes: None,
            resource: None,
            events: None,
            received: time,
        }
    }

    /// Trace 1 (`notes`, 900 ms, its app's span in error), 2 (`notes`, 5 ms) and 3 (`blog`,
    /// 30 ms) in the last 15 minutes, the newest last; 4 an hour ago. Only 1 has an app's span.
    async fn seeded() -> TestApp {
        let app = TestApp::new().await;
        app.logs
            .append(&LogBatch {
                requests: vec![
                    request(4, 3_600, "notes", "/old", 10),
                    request(1, 120, "notes", "/api/documents", 900),
                    request(2, 60, "notes", "/healthz", 5),
                    request(3, 30, "blog", "/", 30),
                ],
                spans: vec![app_span(1, "SELECT documents", SpanStatus::Error)],
                ..LogBatch::default()
            })
            .await
            .unwrap();
        app
    }

    async fn page(app: &TestApp, cookie: &str, uri: &str) -> String {
        let response = app.get(uri, Some(cookie)).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        body(response).await
    }

    /// The root span names of the rows, in page order.
    fn roots(html: &str) -> Vec<&str> {
        html.split(r#"<td class="trace-root">"#)
            .skip(1)
            .map(|rest| {
                rest.split(r#"title=""#)
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap()
            })
            .collect()
    }

    /// The `<tr>` of the row named `root`.
    fn row<'a>(html: &'a str, root: &str) -> &'a str {
        let at = html.find(&format!(r#"title="{root}""#)).unwrap();
        let start = html[..at].rfind("<tr").unwrap();
        let end = at + html[at..].find("</tr>").unwrap();
        &html[start..end]
    }

    #[tokio::test]
    async fn the_scatter_plots_the_range_and_stack_and_links_the_listed_traces() {
        let app = seeded().await;
        let browser = app.sign_in().await;
        let html = page(&app, &browser.cookie, "/telemetry/traces?status=error").await;
        assert!(
            html.contains(r#"<h2 id="scatter-title">Trace duration <span class="card-count">· root spans · last 15m · log scale</span></h2>"#),
            "{html}"
        );
        // Every root of the range, whatever the list's filters; the error listed on top.
        assert!(
            html.contains(r#"aria-label="Trace durations of root spans over the last 15m, on a log scale up to 1 s: 3 traces; slowest error GET /api/documents, 900 ms""#),
            "{html}"
        );
        assert_eq!(html.matches(r#"<circle class="ts-dot ts-ok""#).count(), 2);
        assert_eq!(
            html.matches(r#"<circle class="ts-dot ts-error""#).count(),
            1
        );
        assert_eq!(html.matches(r#"<a class="ts-link""#).count(), 1);
        assert!(
            html.contains(&format!(
                r#"<a class="ts-link" href="/telemetry/traces?status=error&#38;trace={}" aria-label="Open trace GET /api/documents, 900 ms">"#,
                trace(1)
            )),
            "{html}"
        );
        assert!(html.contains(r#"<ul class="chart-legend ts-legend""#));

        // Picked, it is drawn larger.
        let uri = format!("/telemetry/traces?status=error&trace={}", trace(1));
        let html = page(&app, &browser.cookie, &uri).await;
        assert!(html.contains(r#"aria-current="true"><title>Open trace GET /api/documents, 900 ms</title><circle class="ts-listed ts-error" cx=""#), "{html}");
        assert!(html.contains(r#"r="7"></circle></a>"#), "{html}");

        // The stack narrows it.
        let html = page(&app, &browser.cookie, "/telemetry/traces?stack=blog").await;
        assert!(html.contains(": 1 trace; no errors"), "{html}");
        assert_eq!(html.matches("<circle ").count(), 2, "{html}");

        // Nothing in the range draws no chart.
        let html = page(&app, &browser.cookie, "/telemetry/traces?stack=nothing").await;
        assert!(!html.contains("trace-scatter"), "{html}");
    }

    #[tokio::test]
    async fn the_list_has_a_row_per_trace_with_its_columns() {
        let app = seeded().await;
        let browser = app.sign_in().await;
        let html = page(&app, &browser.cookie, "/telemetry/traces").await;
        // The last 15 minutes by default, newest first.
        assert_eq!(
            roots(&html),
            ["GET /", "GET /healthz", "GET /api/documents"],
            "{html}"
        );
        assert!(html.contains("· 3 of 3 · newest first"), "{html}");

        let one = row(&html, "GET /api/documents");
        assert!(
            one.contains(r#"<td class="trace-stack">notes</td>"#),
            "{one}"
        );
        // The proxy's colour first, then the app's from the palette.
        assert!(
            one.contains(r#"<span class="trace-svc"><span class="dot svc-hoster" aria-hidden="true"></span>hoster</span><span class="trace-svc"><span class="dot svc-0" aria-hidden="true"></span>api</span>"#),
            "{one}"
        );
        assert!(!one.contains("app not instrumented"), "{one}");
        assert!(one.contains(r#"<td class="num">3</td>"#), "{one}");
        assert!(one.contains("<span>900 ms</span>"), "{one}");
        // The slowest listed fills its bar; in error, red.
        assert!(one.contains(r#"class="meter meter-fail""#), "{one}");
        assert!(
            one.contains(r#"<rect class="meter-fill" width="100""#),
            "{one}"
        );
        assert!(
            one.contains(r#"<td class="trace-status trace-error"><span class="dot" aria-hidden="true"></span>error</td>"#),
            "{one}"
        );

        let two = row(&html, "GET /healthz");
        assert!(two.contains("app not instrumented"), "{two}");
        assert!(two.contains(r#"<td class="num">2</td>"#), "{two}");
        assert!(two.contains(r#"class="meter meter-neutral""#), "{two}");
        assert!(
            two.contains(r#"<td class="trace-status trace-ok">"#),
            "{two}"
        );
        assert!(row(&html, "GET /").contains("app not instrumented"));

        // The header: the receiver, the rates, the spans' retention, both tabs with counts.
        assert!(
            html.contains(r#"<nav class="crumb" aria-label="Breadcrumb">~/telemetry/traces</nav>"#),
            "{html}"
        );
        assert!(html.contains("otlp receiver · http :80</span>"), "{html}");
        assert!(!html.contains("4317"), "no gRPC port");
        assert!(html.contains(" spans/s · "), "{html}");
        assert!(html.contains("retention 3d · "), "{html}");
        assert!(
            html.contains(r#"<a role="tab" href="/telemetry/traces" aria-selected="true" aria-current="page">Traces <span class="tab-count">3</span></a>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<a role="tab" href="/telemetry/logs" aria-selected="false">Logs <span class="tab-count">0</span></a>"#),
            "{html}"
        );
        // The presets stop at a day: spans are kept 3 days.
        assert!(
            html.contains(r#"<a class="log-preset" href="/telemetry/traces?since=15m" aria-current="true">15m</a>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"href="/telemetry/traces?since=24h""#),
            "{html}"
        );
        assert!(!html.contains("since=7d"), "{html}");
        assert!(!html.contains("style="), "no inline styles");
    }

    #[tokio::test]
    async fn the_list_sorts() {
        let app = seeded().await;
        let browser = app.sign_in().await;
        let html = page(&app, &browser.cookie, "/telemetry/traces?sort=slowest").await;
        assert_eq!(
            roots(&html),
            ["GET /api/documents", "GET /", "GET /healthz"]
        );
        assert!(html.contains("· 3 of 3 · slowest first"), "{html}");
        assert!(
            html.contains(r#"<option value="slowest" selected>slowest first</option>"#),
            "{html}"
        );
        let html = page(&app, &browser.cookie, "/telemetry/traces?sort=errors").await;
        assert_eq!(
            roots(&html),
            ["GET /api/documents", "GET /", "GET /healthz"]
        );
        // Links keep the sort.
        assert!(
            html.contains(r#"href="/telemetry/traces?sort=errors&#38;since=1h""#),
            "{html}"
        );
        let html = page(&app, &browser.cookie, "/telemetry/traces?sort=fastest").await;
        assert_eq!(
            roots(&html),
            ["GET /", "GET /healthz", "GET /api/documents"]
        );
        assert!(html.contains("`fastest` is not a sort"), "{html}");
    }

    #[tokio::test]
    async fn filters_are_get_parameters() {
        let app = seeded().await;
        let browser = app.sign_in().await;
        let cookie = &browser.cookie;
        let get = |uri: &'static str| page(&app, cookie, uri);

        assert_eq!(
            roots(&get("/telemetry/traces?status=error").await),
            ["GET /api/documents"]
        );
        let html = get("/telemetry/traces?status=ok").await;
        assert_eq!(roots(&html), ["GET /", "GET /healthz"]);
        assert!(html.contains("· 2 of 2 · newest first"), "{html}");
        assert!(
            html.contains("Traces <span class=\"tab-count\">3</span>"),
            "the tab counts the range"
        );
        assert!(
            html.contains(r#"<a class="log-chip" href="/telemetry/traces?status=ok" aria-current="page"><span class="dot dot-ok" aria-hidden="true"></span>ok</a>"#),
            "{html}"
        );
        assert_eq!(
            roots(&get("/telemetry/traces?slower=20ms").await),
            ["GET /", "GET /api/documents"]
        );
        // Grep on span names: the root's, or an app's span's.
        assert_eq!(
            roots(&get("/telemetry/traces?q=healthz").await),
            ["GET /healthz"]
        );
        assert_eq!(
            roots(&get("/telemetry/traces?q=select").await),
            ["GET /api/documents"]
        );
        let html = get("/telemetry/traces?stack=blog").await;
        assert_eq!(roots(&html), ["GET /"]);
        assert!(
            html.contains(r#"<option value="blog" selected>blog</option>"#),
            "{html}"
        );
        assert!(
            !html.contains(r#"value="_hoster""#),
            "hoster's records have no traces"
        );
        // The tabs keep the stack and the range.
        let html = get("/telemetry/traces?stack=notes&since=6h").await;
        assert_eq!(
            roots(&html),
            ["GET /healthz", "GET /api/documents", "GET /old"]
        );
        assert!(
            html.contains(r#"<a role="tab" href="/telemetry/logs?stack=notes&#38;since=6h" aria-selected="false">"#),
            "{html}"
        );

        // Nothing matches: the filters are named, with a way out.
        let html = get("/telemetry/traces?status=error&q=healthz").await;
        assert!(roots(&html).is_empty());
        assert!(
            html.contains(r#"No traces match grep “healthz”, status error in this range. <a href="/telemetry/traces">clear filters</a>"#),
            "{html}"
        );
        // An option that does not parse is named and left out.
        let html = get("/telemetry/traces?slower=soon").await;
        assert_eq!(roots(&html).len(), 3);
        assert!(html.contains("`soon` is not a duration"), "{html}");
    }

    #[tokio::test]
    async fn a_trace_is_picked_by_its_id() {
        let app = seeded().await;
        let browser = app.sign_in().await;
        let id = trace(1).to_string();
        let html = page(
            &app,
            &browser.cookie,
            &format!("/telemetry/traces?sort=slowest&trace={id}"),
        )
        .await;
        let one = row(&html, "GET /api/documents");
        assert!(
            one.starts_with(r#"<tr class="trace-row" aria-current="true">"#),
            "{one}"
        );
        assert!(!row(&html, "GET /healthz").contains("aria-current"));
        // Rows pick their trace and keep the other options.
        assert!(
            row(&html, "GET /healthz").contains(&format!(
                r#"href="/telemetry/traces?sort=slowest&#38;trace={}""#,
                trace(2)
            )),
            "{html}"
        );
        // The trace below the list, as its waterfall.
        let picked = &html[html.find(r#"<section class="card trace-picked""#).unwrap()..];
        assert!(
            picked.contains(r#"<h2 id="trace-title">GET /api/documents</h2>"#),
            "{picked}"
        );
        assert!(
            picked.contains(&format!(
                r#"<span class="wf-meta"><code>{id}</code> · <time"#
            )),
            "{picked}"
        );
        assert!(picked.contains(r#"<dt>Spans</dt><dd>3</dd>"#), "{picked}");
        assert!(picked.contains(r#"<dt>Depth</dt><dd>3</dd>"#), "{picked}");
        assert!(
            picked.contains(r#"<dt>Errors</dt><dd class="wf-errors">1</dd>"#),
            "{picked}"
        );
        assert!(
            picked.contains(r#"<li class="svc-0"><span class="wf-swatch" aria-hidden="true"></span>api <span class="muted">1 span</span></li>"#),
            "{picked}"
        );
        assert!(picked.contains(r#"<svg class="wf-minimap""#), "{picked}");
        assert_eq!(picked.matches(r#"<div class="wf-row">"#).count(), 3);
        // Rows in depth-first order: the proxy's two, then the app's span in error.
        let server = picked
            .find("wf-text\" title=\"GET /api/documents\"")
            .unwrap();
        let client = picked.find("wf-text\" title=\"upstream api\"").unwrap();
        let app_span = picked.find("wf-text\" title=\"SELECT documents\"").unwrap();
        assert!(server < client && client < app_span, "{picked}");
        let span = SpanId::from([51; 8]);
        assert!(
            picked.contains(&format!(
                r#"<li id="span-{span}" class="svc-0 wf-error wf-critical">"#
            )),
            "{picked}"
        );
        assert!(
            picked.contains(&format!(
                r##"<a class="wf-pick" href="/telemetry/traces?sort=slowest&#38;trace={id}&#38;span={span}#span-{span}">"##
            )),
            "{picked}"
        );
        assert!(
            picked.contains(r#"<span class="wf-name wf-d2">"#),
            "{picked}"
        );
        assert!(picked.contains(r#"<rect class="wf-ring""#), "{picked}");
        assert!(!picked.contains("aria-current"), "no span selected");
        assert!(
            picked
                .contains(r#"<a class="wf-close" href="/telemetry/traces?sort=slowest">close</a>"#),
            "{picked}"
        );
        assert!(!html.contains("style="), "no inline styles");

        // A span selected: its row marked, linking to clear it; picking another trace or
        // closing drops it, a filter keeps it.
        let html = page(
            &app,
            &browser.cookie,
            &format!("/telemetry/traces?sort=slowest&trace={id}&span={span}"),
        )
        .await;
        assert!(
            html.contains(&format!(
                r##"<a class="wf-pick" href="/telemetry/traces?sort=slowest&#38;trace={id}#span-{span}" aria-current="true">"##
            )),
            "{html}"
        );
        assert_eq!(html.matches(r#"class="wf-pick" href"#).count(), 3);
        let picked = &html[html.find(r#"<section class="card trace-picked""#).unwrap()..];
        assert_eq!(picked.matches("aria-current").count(), 1, "{picked}");
        assert!(
            row(&html, "GET /healthz").contains(&format!(
                r#"href="/telemetry/traces?sort=slowest&#38;trace={}""#,
                trace(2)
            )),
            "{html}"
        );
        assert!(
            html.contains(r#"<a class="wf-close" href="/telemetry/traces?sort=slowest">close</a>"#),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                r#"<input type="hidden" name="span" value="{span}">"#
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                r#"href="/telemetry/traces?sort=slowest&#38;trace={id}&#38;span={span}&#38;since=1h""#
            )),
            "a preset keeps the span: {html}"
        );
        let html = page(
            &app,
            &browser.cookie,
            &format!("/telemetry/traces?trace={id}&span=nope"),
        )
        .await;
        assert!(html.contains("`nope` is not a span id"), "{html}");
        // A filter keeps the trace picked.
        assert!(
            html.contains(&format!(
                r#"<input type="hidden" name="trace" value="{id}">"#
            )),
            "{html}"
        );

        // One that is not there any more.
        let gone = trace(9).to_string();
        let html = page(
            &app,
            &browser.cookie,
            &format!("/telemetry/traces?trace={gone}"),
        )
        .await;
        assert!(
            !html.contains(r#"<tr class="trace-row" aria-current"#),
            "{html}"
        );
        assert!(
            html.contains(
                "This trace does not exist. Spans are kept 3 days, so it may have expired."
            ),
            "{html}"
        );
        // Not a trace id.
        let html = page(&app, &browser.cookie, "/telemetry/traces?trace=nope").await;
        assert!(html.contains("`nope` is not a trace id"), "{html}");
        assert!(!html.contains("trace-picked"), "{html}");
    }

    /// The mockup's `t1` (`docs/design/Telemetry.dc.html`) as trace 7: the proxy's request
    /// row (s0, s1) and the app's spans under its upstream span, s2 the app's server span.
    async fn seeded_t1() -> (TestApp, Vec<(&'static str, SpanId)>) {
        let app = TestApp::new().await;
        let row = request(7, 90, "notes", "/api/documents.info", 1_912);
        let at = MicroTimestamp::from(row.time);
        let ids: Vec<(&str, SpanId)> = [
            ("s0", [7; 8]),
            ("s1", [107; 8]),
            ("s2", [202; 8]),
            ("s3", [203; 8]),
            ("s4", [204; 8]),
            ("s5", [205; 8]),
            ("s6", [206; 8]),
            ("s12", [212; 8]),
            ("s7", [207; 8]),
            ("s8", [208; 8]),
            ("s9", [209; 8]),
            ("s10", [210; 8]),
        ]
        .into_iter()
        .map(|(name, bytes)| (name, SpanId::from(bytes)))
        .collect();
        let id = |name: &str| ids.iter().find(|(n, _)| *n == name).unwrap().1;
        let span = |name: &str, parent: &str, kind, start: u64, micros: u64| Span {
            span_id: id(name),
            parent: Some(id(parent)),
            name: name.into(),
            kind,
            start: at + Duration::from_micros(start),
            duration: Duration::from_micros(micros),
            status: SpanStatus::Unset,
            ..app_span(7, name, SpanStatus::Unset)
        };
        let spans = vec![
            span("s2", "s1", SpanKind::Server, 3_800, 1_905_200),
            span("s3", "s2", SpanKind::Internal, 5_100, 17_600),
            span("s4", "s3", SpanKind::Client, 6_900, 1_800),
            span("s5", "s3", SpanKind::Client, 10_400, 9_200),
            span("s6", "s2", SpanKind::Internal, 23_900, 1_880_300),
            span("s12", "s6", SpanKind::Client, 24_600, 900),
            span("s7", "s6", SpanKind::Client, 26_200, 14_100),
            span("s8", "s6", SpanKind::Client, 41_000, 8_300),
            span("s9", "s6", SpanKind::Client, 51_700, 1_800_400),
            span("s10", "s6", SpanKind::Client, 1_853_600, 2_700),
        ];
        app.logs
            .append(&LogBatch {
                requests: vec![row],
                spans,
                ..LogBatch::default()
            })
            .await
            .unwrap();
        (app, ids)
    }

    /// The span names of the waterfall's rows, in order.
    fn span_rows(html: &str) -> Vec<&str> {
        html.split(r#"<span class="wf-text" title=""#)
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect()
    }

    /// The `<li>` of the span `id`.
    fn span_row(html: &str, id: SpanId) -> &str {
        let at = html.find(&format!(r#"<li id="span-{id}""#)).unwrap();
        let end = at + html[at..].find("</li>").unwrap();
        &html[at..end]
    }

    #[tokio::test]
    async fn spans_collapse_and_the_critical_path_is_highlighted_by_links() {
        let (app, ids) = seeded_t1().await;
        let id = |name: &str| ids.iter().find(|(n, _)| *n == name).unwrap().1;
        let browser = app.sign_in().await;
        let trace = trace(7);
        let base = format!("/telemetry/traces?trace={trace}");
        let html = page(&app, &browser.cookie, &base).await;
        assert!(!html.contains("style="), "no inline styles");
        assert_eq!(span_rows(&html).len(), 12, "nothing collapsed");
        // A span with children has a toggle; a leaf a gap.
        let s6 = span_row(&html, id("s6"));
        assert!(
            s6.contains(&format!(
                r##"<a class="wf-toggle" href="/telemetry/traces?trace={trace}&#38;collapsed={}#span-{}" aria-expanded="true" aria-label="Collapse s6">"##,
                id("s6"),
                id("s6")
            )),
            "{s6}"
        );
        assert!(!s6.contains("wf-hidden"), "{s6}");
        let s9 = span_row(&html, id("s9"));
        assert!(
            !s9.contains("wf-toggle\"") && s9.contains("wf-toggle-gap"),
            "{s9}"
        );
        // The critical path: s0, s1, s2, s6, s10.
        let critical: Vec<&str> = ids
            .iter()
            .filter(|(_, span)| span_row(&html, *span).contains("wf-critical"))
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(critical, ["s0", "s1", "s2", "s6", "s10"]);
        // The controls.
        assert!(
            html.contains(&format!(
                r##"<a class="button wf-control" href="/telemetry/traces?trace={trace}#trace-title">expand all</a>"##
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                r##"<a class="button wf-control" href="/telemetry/traces?trace={trace}&#38;collapsed=services#trace-title">collapse to services</a>"##
            )),
            "{html}"
        );
        assert!(
            html.contains(r#"<input type="checkbox" role="switch" class="wf-crit-input">"#),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                r##"<a class="switch wf-crit-link" role="switch" aria-checked="false" href="/telemetry/traces?trace={trace}&#38;crit=on#trace-title">"##
            )),
            "{html}"
        );
        assert!(html.contains(r#"<div class="waterfall">"#), "{html}");

        // Collapse to services: s2 is the only server span with children and a parent.
        let html = page(&app, &browser.cookie, &format!("{base}&collapsed=services")).await;
        assert_eq!(
            span_rows(&html),
            ["GET /api/documents.info", "upstream api", "s2"]
        );
        let s2 = span_row(&html, id("s2"));
        assert!(
            s2.contains(r#"aria-expanded="false" aria-label="Expand s2">"#),
            "{s2}"
        );
        assert!(
            s2.contains(r#"<span class="wf-hidden">+9<span class="visually-hidden"> spans hidden</span></span>"#),
            "{s2}"
        );
        // Expanding s2 again toggles it from the rule; the URL stays short.
        assert!(
            s2.contains(&format!(
                r#"href="/telemetry/traces?trace={trace}&#38;collapsed=services%2C{}#span-{}""#,
                id("s2"),
                id("s2")
            )),
            "{s2}"
        );
        // Selecting a span keeps the collapse; picking another trace and closing drop it.
        assert!(
            s2.contains(&format!(
                r#"<a class="wf-pick" href="/telemetry/traces?trace={trace}&#38;collapsed=services&#38;span={}#span-{}">"#,
                id("s2"),
                id("s2")
            )),
            "{s2}"
        );
        assert!(
            html.contains(r#"<a class="wf-close" href="/telemetry/traces">close</a>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<input type="hidden" name="collapsed" value="services">"#),
            "a filter keeps it: {html}"
        );

        // The round trip: services with s2 expanded and s6 collapsed, given in any order and
        // with a leaf, comes back canonical.
        let given = format!(
            "{base}&collapsed={},services,{},{}",
            id("s6"),
            id("s9"),
            id("s2")
        );
        let html = page(&app, &browser.cookie, &given).await;
        assert_eq!(
            span_rows(&html),
            [
                "GET /api/documents.info",
                "upstream api",
                "s2",
                "s3",
                "s4",
                "s5",
                "s6"
            ]
        );
        let s6 = span_row(&html, id("s6"));
        assert!(s6.contains(">+5<"), "{s6}");
        assert!(
            s6.contains(&format!(
                r#"href="/telemetry/traces?trace={trace}&#38;collapsed=services%2C{}#span-{}" aria-expanded="false" aria-label="Expand s6">"#,
                id("s2"),
                id("s6")
            )),
            "{s6}"
        );
        let s3 = span_row(&html, id("s3"));
        assert!(
            s3.contains(&format!(
                r#"collapsed=services%2C{}%2C{}%2C{}#span-{}""#,
                id("s2"),
                id("s3"),
                id("s6"),
                id("s3")
            )),
            "{s3}"
        );
        assert!(
            html.contains(&format!(
                r##"href="/telemetry/traces?trace={trace}#trace-title">expand all</a>"##
            )),
            "{html}"
        );
        let html = page(&app, &browser.cookie, &format!("{base}&collapsed=bogus")).await;
        assert!(
            html.contains(r#"unknown span id &#34;bogus&#34;"#),
            "{html}"
        );
        assert_eq!(span_rows(&html).len(), 12);

        // The critical path highlighted by the server: the class, the switch on, and the link
        // turning it off.
        let html = page(&app, &browser.cookie, &format!("{base}&crit=on")).await;
        assert!(
            html.contains(r#"<div class="waterfall wf-crit-on">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<input type="checkbox" role="switch" class="wf-crit-input" checked>"#),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                r##"role="switch" aria-checked="true" href="/telemetry/traces?trace={trace}#trace-title">"##
            )),
            "{html}"
        );
        // Collapse links keep it.
        assert!(
            html.contains(&format!(
                r##"href="/telemetry/traces?trace={trace}&#38;crit=on&#38;collapsed=services#trace-title">collapse to services"##
            )),
            "{html}"
        );
    }

    #[tokio::test]
    async fn the_list_pages() {
        let app = TestApp::new().await;
        let requests = (1..=55)
            .map(|n| request(n, 600 - u64::from(n), "notes", &format!("/{n}"), 5))
            .collect();
        app.logs
            .append(&LogBatch {
                requests,
                ..LogBatch::default()
            })
            .await
            .unwrap();
        let browser = app.sign_in().await;
        let html = page(&app, &browser.cookie, "/telemetry/traces?status=ok").await;
        assert_eq!(roots(&html).len(), 50);
        assert_eq!(roots(&html)[0], "GET /55");
        assert!(html.contains("· 50 of 55 · newest first"), "{html}");
        assert!(
            html.contains(
                r#"<a href="/telemetry/traces?status=ok&#38;page=2" rel="next">next →</a>"#
            ),
            "{html}"
        );
        let html = page(&app, &browser.cookie, "/telemetry/traces?status=ok&page=2").await;
        assert_eq!(
            roots(&html),
            ["GET /5", "GET /4", "GET /3", "GET /2", "GET /1"]
        );
        assert!(
            html.contains(r#"<a href="/telemetry/traces?status=ok" rel="prev">← previous</a>"#),
            "{html}"
        );
        // Picking a trace keeps the page.
        assert!(
            html.contains(&format!(
                r#"href="/telemetry/traces?status=ok&#38;page=2&#38;trace={}""#,
                trace(3)
            )),
            "{html}"
        );
    }

    #[tokio::test]
    async fn the_logs_tab_counts_the_traces_of_its_range() {
        let app = seeded().await;
        let browser = app.sign_in().await;
        let html = page(&app, &browser.cookie, "/telemetry/logs?stack=notes").await;
        assert!(
            html.contains(r#"<a role="tab" href="/telemetry/traces?stack=notes" aria-selected="false">Traces <span class="tab-count">2</span></a>"#),
            "{html}"
        );
    }
}
