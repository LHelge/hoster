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
async fn the_page_redirects_to_its_logs_tab() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app.get("/telemetry", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/telemetry/logs");
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
