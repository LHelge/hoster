//! The event row and the Recent events card rendered on their own, then the Events page, the
//! stack overview's card and its live stream through the whole app against the fake
//! orchestrator.

use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use http_body_util::BodyExt as _;

use super::*;
use crate::orchestrator::UiEvent;
use crate::store::{EventId, EventKind, Timestamp};
use crate::web::auth::tests::{Browser, TestApp};
use crate::web::stacks::tests::body;

const NOW: Duration = Duration::from_secs(1_800_000_000);

fn now() -> SystemTime {
    UNIX_EPOCH + NOW
}

/// Event `id`, `ago` before [`now`].
fn event(id: i64, ago: Duration, severity: Severity) -> Event {
    Event {
        id: EventId::from(id),
        at: Timestamp::from(now() - ago),
        kind: EventKind::DeploySucceeded,
        severity,
        stack: Some("notes".into()),
        subject: Some(format!("/stacks/notes/deployments/{id}")),
        message: format!("notes deployed <#{id}>"),
        meta: Some("a91f3c2 · webhook".into()),
    }
}

fn row(event: &Event) -> String {
    EventRow::new(event, now()).render().unwrap()
}

fn compact(html: &str) -> String {
    html.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn each_severity_has_its_tone_dot_and_says_it_in_words() {
    for (severity, tone) in [
        (Severity::Info, "progress"),
        (Severity::Success, "ok"),
        (Severity::Warning, "warn"),
        (Severity::Error, "fail"),
    ] {
        let html = row(&event(1, Duration::from_secs(300), severity));
        assert!(
            html.contains(&format!(
                r#"<span class="dot event-dot dot-{tone}" aria-hidden="true"></span>"#
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                r#"<span class="visually-hidden">{severity}: </span>"#
            )),
            "{html}"
        );
        assert!(!html.contains("style="), "{html}");
    }
}

#[test]
fn a_row_links_its_subject_and_shows_its_time_and_meta() {
    let html = compact(&row(&event(7, Duration::from_secs(300), Severity::Success)));
    assert!(
        html.contains(r#"<time class="event-time" datetime="2027-01-15T07:55:00Z" title="2027-01-15T07:55:00Z">5 min ago</time>"#),
        "{html}"
    );
    // The message is escaped like any text.
    assert!(
        html.contains(r#"<a href="/stacks/notes/deployments/7">notes deployed &#60;#7&#62;</a>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="event-meta">a91f3c2 · webhook</span>"#),
        "{html}"
    );
}

#[test]
fn a_row_without_a_page_to_link_is_plain_text() {
    for subject in [None, Some("notes"), Some("//evil.example/x"), Some("")] {
        let mut event = event(1, Duration::ZERO, Severity::Info);
        event.subject = subject.map(str::to_owned);
        event.meta = None;
        let html = row(&event);
        assert!(!html.contains("<a "), "{subject:?}: {html}");
        assert!(
            html.contains("notes deployed &#60;#1&#62;</span>"),
            "{html}"
        );
        assert!(html.contains(">just now</time>"), "{html}");
        assert!(!html.contains("event-meta"), "{html}");
    }
}

#[test]
fn an_empty_card_says_so() {
    let html = RecentEvents::new(Some(Vec::new()), now()).render().unwrap();
    assert!(html.contains("No events yet."), "{html}");
    assert!(!html.contains("events-feed"), "{html}");

    let html = RecentEvents::new(None, now()).render().unwrap();
    assert!(
        html.contains("The events cannot be read right now."),
        "{html}"
    );
}

#[test]
fn the_card_keeps_the_newest_events_first() {
    let events: Vec<Event> = (1..=12)
        .map(|id| event(id, Duration::from_secs(600 - id as u64), Severity::Info))
        .rev()
        .collect();
    let mut card = RecentEvents::new(Some(events), now());
    let html = card.render().unwrap();
    assert_eq!(
        html.matches("<li class=\"event-row\">").count(),
        RecentEvents::LEN
    );
    assert!(html.contains("/deployments/12\""), "{html}");
    assert!(!html.contains("/deployments/2\""), "{html}");

    // A new event goes on top and the oldest falls off; one already shown is not doubled.
    let newer = event(13, Duration::ZERO, Severity::Error);
    assert!(card.add([
        newer.clone(),
        event(12, Duration::from_secs(588), Severity::Info)
    ]));
    let html = card.partial(now());
    assert!(
        html.starts_with(r##"<hx-partial hx-target="#recent-events" hx-swap="outerHTML"><section class="card" id="recent-events""##),
        "{html}"
    );
    assert!(html.ends_with("</section></hx-partial>"), "{html}");
    assert_eq!(
        html.matches("<li class=\"event-row\">").count(),
        RecentEvents::LEN
    );
    assert_eq!(html.matches("/deployments/12\"").count(), 1, "{html}");
    let first = html.find("/deployments/13\"").unwrap();
    assert!(first < html.find("/deployments/12\"").unwrap(), "{html}");
    assert!(!html.contains("/deployments/3\""), "{html}");
}

#[test]
fn an_unreadable_card_cannot_be_added_to() {
    let mut card = RecentEvents::new(None, now());
    assert!(!card.add([event(1, Duration::ZERO, Severity::Info)]));
}

#[test]
fn a_stacks_card_keeps_to_its_stack_and_links_to_its_events() {
    let mut other = event(2, Duration::ZERO, Severity::Info);
    other.stack = Some("blog".into());
    let mut card = RecentEvents::of_stack(
        "notes",
        Some(vec![event(1, Duration::from_secs(60), Severity::Info)]),
        now(),
    );
    assert!(card.add([other, event(3, Duration::ZERO, Severity::Info)]));
    let html = compact(&card.render().unwrap());
    assert!(html.contains("/deployments/1\""), "{html}");
    assert!(html.contains("/deployments/3\""), "{html}");
    assert!(!html.contains("/deployments/2\""), "{html}");
    assert!(
        html.contains(
            r#"<a class="card-link" href="/activity?stack=notes">all events of this stack →</a>"#
        ),
        "{html}"
    );

    let html = compact(&RecentEvents::default().render().unwrap());
    assert!(
        html.contains(r#"<a class="card-link" href="/activity">view all →</a>"#),
        "{html}"
    );
    let html = RecentEvents::of_stack("notes", Some(Vec::new()), now())
        .render()
        .unwrap();
    assert!(html.contains("changes to this stack"), "{html}");
}

/// Event `id` of `stack` (none when empty), of `kind` and `severity`, `id` seconds after an
/// hour ago, so a higher id is newer.
fn logged(id: i64, stack: &str, kind: EventKind, severity: Severity) -> Event {
    Event {
        id: EventId::from(id),
        at: Timestamp::from(
            SystemTime::now() - Duration::from_secs(3600) + Duration::from_secs(id as u64),
        ),
        kind,
        severity,
        stack: (!stack.is_empty()).then(|| stack.to_owned()),
        subject: None,
        message: format!("event #{id} of {stack}"),
        meta: None,
    }
}

async fn page(app: &TestApp, browser: &Browser, url: &str, expect: StatusCode) -> String {
    let response = app.get(url, Some(&browser.cookie)).await;
    assert_eq!(response.status(), expect, "{url}");
    compact(&body(response).await)
}

/// The ids the page shows, in order.
fn shown(html: &str) -> Vec<i64> {
    html.match_indices("event #")
        .map(|(at, _)| {
            let digits: String = html[at + 7..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().unwrap()
        })
        .collect()
}

#[tokio::test]
async fn the_events_page_filters_by_kind_severity_and_stack() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    *app.stacks.log.lock().unwrap() = vec![
        logged(1, "notes", EventKind::DeploySucceeded, Severity::Success),
        logged(2, "notes", EventKind::ContainerRestarted, Severity::Warning),
        logged(3, "", EventKind::CertificateFailed, Severity::Error),
        logged(4, "blog", EventKind::DeployFailed, Severity::Error),
        logged(5, "", EventKind::HosterStarted, Severity::Info),
        // blog is not a stack any more: its events stay, and stay filterable by its name.
        logged(6, "blog", EventKind::StackRemoved, Severity::Info),
    ];

    let html = page(&app, &browser, "/activity", StatusCode::OK).await;
    assert_eq!(shown(&html), [6, 5, 4, 3, 2, 1], "{html}");
    assert!(html.contains("<title>Events · hoster</title>"), "{html}");
    assert!(
        html.contains(r#"<nav class="crumb" aria-label="Breadcrumb">~/activity</nav>"#),
        "{html}"
    );
    // The design gives the page no navigation entry, so none is current.
    let nav = &html[html.find(r#"<ul class="nav">"#).expect(&html)..];
    let nav = &nav[..nav.find("</ul>").unwrap()];
    assert!(!nav.contains("aria-current"), "{nav}");
    assert!(!nav.contains("/activity"), "{nav}");
    assert!(
        html.contains(r#"<a class="filter" href="/activity" aria-current="page">all</a>"#),
        "{html}"
    );
    assert!(
        html.contains(
            r#"<a class="filter" href="/activity?kind=operator">operator &#38; system</a>"#
        ),
        "{html}"
    );
    assert!(html.contains(r#"<option value="notes">"#), "{html}");
    assert!(!html.contains("event-pager"), "one page: {html}");

    for (url, ids) in [
        ("/activity?kind=deploys", vec![4, 1]),
        ("/activity?kind=containers", vec![2]),
        ("/activity?kind=certificates", vec![3]),
        ("/activity?kind=operator", vec![6, 5]),
        ("/activity?severity=warning", vec![4, 3, 2]),
        ("/activity?severity=error&kind=deploys", vec![4]),
        ("/activity?stack=notes", vec![2, 1]),
        ("/activity?stack=blog", vec![6, 4]),
        ("/activity?stack=blog&kind=operator", vec![6]),
        ("/activity?stack=nobody", vec![]),
        // Unknown and empty filter values are ignored, as on /stacks.
        (
            "/activity?kind=nonsense&severity=fatal&stack=",
            vec![6, 5, 4, 3, 2, 1],
        ),
    ] {
        let html = page(&app, &browser, url, StatusCode::OK).await;
        assert_eq!(shown(&html), ids, "{url}: {html}");
    }

    let html = page(
        &app,
        &browser,
        "/activity?stack=blog&severity=error",
        StatusCode::OK,
    )
    .await;
    assert!(
        html.contains("<h2 id=\"events-title\">Events of blog</h2>"),
        "{html}"
    );
    // The stack form keeps the other filters; the filter links keep the stack.
    assert!(
        html.contains(r#"<input type="hidden" name="severity" value="error">"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<input type="text" name="stack" value="blog""#),
        "{html}"
    );
    assert!(
        html.contains(
            r#"href="/activity?kind=deploys&#38;severity=error&#38;stack=blog">deploys</a>"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/activity?severity=error">every stack</a>"#),
        "{html}"
    );

    let html = page(&app, &browser, "/activity?stack=nobody", StatusCode::OK).await;
    assert!(html.contains("No events match these filters."), "{html}");
    assert!(
        html.contains(r#"<a href="/activity">Show every event</a>"#),
        "{html}"
    );
}

#[tokio::test]
async fn the_events_page_pages_by_cursor() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let page_len = EventQuery::PAGE as i64;
    *app.stacks.log.lock().unwrap() = (1..=page_len)
        .map(|id| logged(id, "notes", EventKind::StackApplied, Severity::Info))
        .collect();

    // Exactly one page: no older link.
    let html = page(&app, &browser, "/activity", StatusCode::OK).await;
    assert_eq!(shown(&html).len(), page_len as usize);
    assert!(!html.contains("older →"), "{html}");

    // One more event: the oldest moves to a second page, linked with the filters kept.
    app.stacks.log.lock().unwrap().push(logged(
        page_len + 1,
        "notes",
        EventKind::StackApplied,
        Severity::Info,
    ));
    let html = page(&app, &browser, "/activity?stack=notes", StatusCode::OK).await;
    let ids = shown(&html);
    assert_eq!(ids.len(), page_len as usize);
    assert_eq!(ids.first(), Some(&(page_len + 1)));
    assert_eq!(ids.last(), Some(&2));
    let start = html
        .find(r#"<nav class="card-foot event-pager" aria-label="Pages">"#)
        .expect(&html);
    let older = &html[start..];
    let href = older
        .split(r#"<a href=""#)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect(older)
        .replace("&#38;", "&");
    assert!(href.starts_with("/activity?stack=notes&before="), "{href}");
    assert!(!html.contains("← newest"), "{html}");

    let html = page(&app, &browser, &href, StatusCode::OK).await;
    assert_eq!(shown(&html), [1], "{html}");
    assert!(!html.contains("older →"), "the last page: {html}");
    assert!(
        html.contains(r#"<a href="/activity?stack=notes">← newest</a>"#),
        "{html}"
    );

    // A cursor that is not one is refused, and the newest events shown under a notice.
    let html = page(
        &app,
        &browser,
        "/activity?before=yesterday",
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        html.contains("That link to older events is not valid"),
        "{html}"
    );
    assert_eq!(shown(&html).len(), page_len as usize);
}

#[tokio::test]
async fn the_events_page_needs_a_session() {
    let app = TestApp::new().await;
    let response = app.get("/activity", None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_dashboard_card_links_to_the_events_page() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = page(&app, &browser, "/", StatusCode::OK).await;
    assert!(
        html.contains(r#"<a class="card-link" href="/activity">view all →</a>"#),
        "{html}"
    );
}

/// Reads `stream` into `text` until it contains `needle`.
async fn read_until(stream: &mut Body, text: &mut String, needle: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !text.contains(needle) {
            let frame = stream
                .frame()
                .await
                .expect("the stream ended early")
                .unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {needle:?} in {text:?}"));
}

#[tokio::test]
async fn the_stack_overview_shows_its_events_live() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    *app.stacks.log.lock().unwrap() = vec![
        logged(1, "notes", EventKind::DeploySucceeded, Severity::Success),
        logged(2, "blog", EventKind::DeployFailed, Severity::Error),
    ];

    let html = page(&app, &browser, "/stacks/notes", StatusCode::OK).await;
    assert!(
        html.contains(r#"<div id="stack-events" hx-sse:connect="/stacks/notes/events" hx-swap="innerHTML" hx-sse:close="end"> <section class="card" id="recent-events""#),
        "{html}"
    );
    assert_eq!(shown(&html), [1], "only notes' events: {html}");
    assert!(
        html.contains(
            r#"<a class="card-link" href="/activity?stack=notes">all events of this stack →</a>"#
        ),
        "{html}"
    );

    let response = app.get("/stacks/notes/events", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut stream = response.into_body();
    let mut text = String::new();
    read_until(&mut stream, &mut text, "\n\n").await;
    assert!(
        text.starts_with(r#"data: <section class="card" id="recent-events""#),
        "{text}"
    );
    assert!(text.contains("event #1 of notes"), "{text}");

    // Another stack's event sends nothing; notes' next one sends the card with it on top.
    let blog = logged(3, "blog", EventKind::StackApplied, Severity::Info);
    app.stacks.log.lock().unwrap().push(blog.clone());
    app.stacks.events.send(UiEvent::Event(blog)).unwrap();
    let notes = logged(4, "notes", EventKind::StackApplied, Severity::Info);
    app.stacks.log.lock().unwrap().push(notes.clone());
    app.stacks.events.send(UiEvent::Event(notes)).unwrap();
    text.clear();
    read_until(&mut stream, &mut text, "\n\n").await;
    assert!(!text.contains("of blog"), "{text}");
    let newest = text.find("event #4 of notes").expect(&text);
    assert!(
        newest < text.find("event #1 of notes").expect(&text),
        "{text}"
    );

    // Signing out replaces the card with a notice and ends the stream.
    let logout = axum::http::Request::post("/auth/logout")
        .header("cookie", &browser.cookie)
        .header("x-csrf-token", &browser.csrf)
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    app.send(logout).await;
    text.clear();
    read_until(&mut stream, &mut text, "event: end").await;
    assert!(
        text.starts_with(r#"data: <p class="flash flash-error">Signed out"#),
        "{text}"
    );
}

#[tokio::test]
async fn an_unknown_stack_has_no_event_stream() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app
        .get("/stacks/nobody/events", Some(&browser.cookie))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app.get("/stacks/notes/events", None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}
