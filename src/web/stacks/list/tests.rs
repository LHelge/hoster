//! The stacks list: conditions, the filter, the cards and their statistics.

use std::time::Duration;

use axum::http::StatusCode;

use super::*;
use crate::health::HealthStatus;
use crate::stats::{Histogram, StatusClass, StatusCounts};
use crate::store::{DeploymentId, Provenance, Resolution};
use crate::web::auth::Session;
use crate::web::auth::tests::TestApp;
use crate::web::dashboard::tests::{MINUTE, bucket, history, hour};
use crate::web::stacks::tests::{body, deployment, notes_status};

/// `notes` with both services running and healthy, its last deployment live, no flag.
fn healthy() -> StackStatus {
    let mut status = notes_status();
    status.flag = None;
    for (n, service) in status.services.iter_mut().enumerate() {
        service.state = ServiceState::Running {
            generation: 2,
            digest: crate::web::stacks::tests::digest(n as u8),
        };
        service.health.status = HealthStatus::Healthy;
    }
    status
}

fn layout() -> Layout {
    Layout::new(Section::Stacks, &Session::for_tests("octocat", "c0ffee"))
}

/// A history over the stack pages' window without any requests.
fn quiet() -> TrafficHistory {
    TrafficHistory::empty(TrafficWindow::RANGE, Timestamp::now())
}

#[test]
fn condition_follows_deployments_then_services() {
    assert_eq!(Condition::from(&healthy()), Condition::Running);

    let mut deploying = healthy();
    deploying.active = Some((DeploymentId::from(4), DeploymentState::Pulling));
    deploying.flag = Some("revision 1: deploy failed: x".into());
    assert_eq!(Condition::from(&deploying), Condition::Deploying);

    // The fixture carries a failed-deploy flag (ADR-0049).
    assert_eq!(Condition::from(&notes_status()), Condition::Failed);
    let mut rejected = healthy();
    rejected.last = Some(deployment(5, DeploymentState::Rejected));
    assert_eq!(Condition::from(&rejected), Condition::Failed);

    let mut crashing = healthy();
    crashing.services[0].restarts.crash_looping = true;
    assert_eq!(Condition::from(&crashing), Condition::Degraded);
    let mut unhealthy = healthy();
    unhealthy.services[1].health.status = HealthStatus::Unhealthy;
    assert_eq!(Condition::from(&unhealthy), Condition::Degraded);
    let mut partial = healthy();
    partial.services[1].state = ServiceState::NotDeployed;
    assert_eq!(Condition::from(&partial), Condition::Degraded);
    let mut warned = healthy();
    warned.last = Some(deployment(5, DeploymentState::LiveWithWarnings));
    assert_eq!(Condition::from(&warned), Condition::Degraded);

    let mut waiting = healthy();
    waiting.last = None;
    for service in &mut waiting.services {
        service.state = ServiceState::AwaitingFirstDigest;
        service.health.status = HealthStatus::Unknown;
    }
    assert_eq!(Condition::from(&waiting), Condition::NotDeployed);
}

#[test]
fn conditions_parse_from_their_query_value() {
    for condition in Condition::ALL {
        assert_eq!(condition.slug().parse::<Condition>(), Ok(condition));
    }
    assert_eq!(Condition::NotDeployed.to_string(), "not deployed");
    assert!("everything".parse::<Condition>().is_err());
}

#[test]
fn the_filter_counts_every_condition_and_shows_only_the_chosen_one() {
    let mut blog = healthy();
    blog.name = "blog".into();
    blog.services[0].restarts.crash_looping = true;
    let stacks = [healthy(), blog, notes_status()];
    let traffic = quiet();
    let traffic = Some(&traffic);

    let all = StackList::new(layout(), &stacks, traffic, None, SystemTime::now());
    let links: Vec<(&str, usize, bool)> = all
        .filters
        .iter()
        .map(|l| (l.label.as_str(), l.count, l.current))
        .collect();
    // "not deployed" only shows while a stack is in it.
    assert_eq!(
        links,
        [
            ("all", 3, true),
            ("running", 1, false),
            ("degraded", 1, false),
            ("deploying", 0, false),
            ("failed", 1, false),
        ]
    );
    assert_eq!(all.filters[2].href, "/stacks?status=degraded");
    assert_eq!(all.cards.len(), 3);

    let degraded = StackList::new(
        layout(),
        &stacks,
        traffic,
        Some(Condition::Degraded),
        SystemTime::now(),
    );
    assert!(degraded.filters[2].current && !degraded.filters[0].current);
    let names: Vec<&str> = degraded.cards.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["blog"]);

    let html = StackList::new(
        layout(),
        &stacks,
        traffic,
        Some(Condition::Deploying),
        SystemTime::now(),
    )
    .render()
    .unwrap();
    assert!(
        html.contains(r#"<a class="filter" href="/stacks?status=deploying" aria-current="page">deploying <span class="filter-count">0</span></a>"#),
        "{html}"
    );
    assert!(
        html.contains("No stack has the status “deploying” right now."),
        "{html}"
    );
    assert!(!html.contains("stack-card\""), "{html}");

    // The rare "not deployed" link is hidden while empty, unless it is the filter chosen.
    let html = StackList::new(
        layout(),
        &stacks,
        traffic,
        Some(Condition::NotDeployed),
        SystemTime::now(),
    )
    .render()
    .unwrap();
    assert!(
        html.contains(r#"<a class="filter" href="/stacks?status=not-deployed" aria-current="page">not deployed <span class="filter-count">0</span></a>"#),
        "{html}"
    );
    assert!(
        html.contains("No stack has the status “not deployed” right now."),
        "{html}"
    );
}

#[test]
fn no_stacks_shows_no_filter_but_the_add_card() {
    let html = StackList::new(layout(), &[], Some(&quiet()), None, SystemTime::now())
        .render()
        .unwrap();
    assert!(html.contains("No stacks yet."), "{html}");
    assert!(!html.contains("Status filter"), "{html}");
    assert!(
        html.contains(r#"<a class="add-stack" href="/apply">"#),
        "{html}"
    );
}

#[test]
fn a_card_shows_the_webhook_commit_the_running_deploy_and_the_queue() {
    let now = SystemTime::now();
    let mut status = healthy();
    let last = status.last.as_mut().unwrap();
    last.source = DeploymentSource::Webhook;
    last.updated_at = Timestamp::from(now - Duration::from_secs(3 * 3_600));
    last.provenance = Some(Provenance {
        repository: "acme/notes".into(),
        sha: "4f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39".into(),
        git_ref: "refs/tags/v1.4.2".into(),
        run_id: "1".into(),
        actor: "octocat".into(),
        tag: None,
    });
    let card = StackCard::new(&status, Condition::Running, Some(&quiet()), now);
    let last = card.last.as_ref().unwrap();
    assert_eq!(last.source, "webhook");
    assert_eq!(last.ago, "3 h ago");
    assert_eq!(
        last.commit,
        Some(Commit {
            short: "4f2a9c1".into(),
            sha: "4f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39".into(),
            repository: "acme/notes".into()
        })
    );
    assert_eq!(card.routes, ["notes.example.com"]);

    // While a deployment runs it replaces the last one, and only its target is deploying.
    status.active = Some((DeploymentId::from(3), DeploymentState::Pulling));
    status.queued = Some(DeploymentId::from(4));
    let card = StackCard::new(&status, Condition::Deploying, Some(&quiet()), now);
    assert!(card.last.is_none());
    assert_eq!(
        card.active,
        Some((
            "3".into(),
            "pulling".into(),
            "/stacks/notes/deployments/3".into()
        ))
    );
    assert_eq!(card.queued.as_deref(), Some("4"));
    let dots: Vec<(&str, Tone)> = card
        .services
        .iter()
        .map(|s| (s.name.as_str(), s.tone))
        .collect();
    assert_eq!(dots, [("api", Tone::Progress), ("storage", Tone::Ok)]);
}

#[test]
fn chips_put_failing_before_deploying() {
    let mut status = healthy();
    status.services[0].health.status = HealthStatus::Unhealthy;
    status.services[1].state = ServiceState::NotDeployed;
    let card = StackCard::new(
        &status,
        Condition::Degraded,
        Some(&quiet()),
        SystemTime::now(),
    );
    let chips: Vec<(Tone, &str, &str)> = card
        .services
        .iter()
        .map(|s| (s.tone, s.word, s.detail.as_str()))
        .collect();
    assert_eq!(chips[0].0, Tone::Fail);
    assert_eq!(chips[0].1, "failing");
    assert!(chips[0].2.ends_with(", unhealthy"), "{}", chips[0].2);
    assert_eq!(chips[1], (Tone::Neutral, "stopped", "not deployed"));
    assert_eq!(card.services[1].shown_word(), Some("stopped"));
    assert_eq!(card.services[1].spoken(), "not deployed");

    status.services[1].state = ServiceState::AwaitingFirstDigest;
    let card = StackCard::new(
        &status,
        Condition::Degraded,
        Some(&quiet()),
        SystemTime::now(),
    );
    assert_eq!(card.services[1].shown_word(), Some("waiting"));
    assert_eq!(card.services[1].spoken(), "awaiting its first digest");
    assert_eq!(card.services[1].tone, Tone::Neutral);
}

/// Totals of `count` requests of each (status class, latency in ms).
fn totals(requests: &[(StatusClass, u64, u64)]) -> TrafficTotals {
    let mut latency = Histogram::default();
    for &(_, count, millis) in requests {
        for _ in 0..count {
            latency.record(Duration::from_millis(millis));
        }
    }
    TrafficTotals {
        statuses: requests
            .iter()
            .map(|&(class, count, _)| (class, count))
            .collect::<StatusCounts>(),
        upstream_errors: 0,
        latency,
    }
}

#[test]
fn a_traffic_line_shares_and_flags_the_figures() {
    use StatusClass::{ServerError, Success};
    // 97 of the 100 requests took 5 ms: three slow ones do not make the stack slow.
    let line = TrafficLine::from(&totals(&[
        (Success, 97, 5),
        (ServerError, 2, 2_000),
        (Success, 1, 2_000),
    ]));
    assert_eq!(line.requests, 100);
    assert_eq!(line.errors, "2.0 %");
    assert_eq!(line.client_errors, "0 %");
    assert_eq!(line.errors_tone, Tone::Fail);
    assert_eq!(line.p95_tone, Tone::Neutral, "{}", line.p95);
    assert_eq!(line.p50, line.p95);

    let slow = TrafficLine::from(&totals(&[(ServerError, 50, 9_000)]));
    assert_eq!(slow.p95_tone, Tone::Warn, "{}", slow.p95);

    let quiet = TrafficLine::from(&TrafficTotals::default());
    assert_eq!((quiet.requests, quiet.p95.as_str()), (0, "-"));
    assert_eq!(
        (quiet.errors.as_str(), quiet.errors_tone),
        ("-", Tone::Neutral)
    );

    // One error in 5000 rounds to 0.0 but is not none.
    let rare = TrafficLine::from(&totals(&[(Success, 4_999, 5), (ServerError, 1, 5)]));
    assert_eq!(
        (rare.errors.as_str(), rare.errors_tone),
        ("<0.1 %", Tone::Neutral)
    );
}

#[tokio::test]
async fn a_card_shows_the_last_hour_of_its_stacks_routes() {
    let notes = ("notes.example.com", "notes", "api");
    let files = ("files.example.com", "notes", "storage");
    let blog = ("blog.example.com", "blog", "web");
    // `now` is 30 min 30 s into the hour: its newest flushed minute is `hour + 29 min`.
    let history = history(
        TrafficWindow::RANGE,
        vec![
            // Before the hour's window: not counted.
            bucket(hour() - MINUTE * 40, notes, &[(500, 900, 5)]),
            bucket(hour() + MINUTE * 3, notes, &[(200, 40, 5)]),
            bucket(hour() + MINUTE * 26, notes, &[(200, 48, 5), (503, 2, 5)]),
            bucket(hour() + MINUTE * 29, files, &[(200, 10, 2_000)]),
            bucket(hour() + MINUTE * 29, blog, &[(500, 70, 5)]),
        ],
    )
    .await;
    let card = StackCard::new(
        &healthy(),
        Condition::Degraded,
        Some(&history),
        SystemTime::now(),
    );
    let CardTraffic::Shown(traffic) = &card.traffic else {
        panic!("{:?}", card.traffic);
    };
    // The last 5 minutes held 60 requests: 12 a minute.
    assert_eq!(traffic.per_minute.to_string(), "12");
    assert_eq!(traffic.line.requests, 100);
    assert_eq!(traffic.line.errors, "2.0 %");
    assert_eq!(traffic.line.errors_tone, Tone::Fail);
    assert_eq!(traffic.line.p95_tone, Tone::Warn, "{}", traffic.line.p95);
    // 60 bars of a minute, in the tone of the stack's condition.
    let spark = traffic.spark.to_string();
    assert!(spark.contains(r#"class="spark spark-warn""#), "{spark}");
    assert_eq!(spark.matches(',').count(), 60, "{spark}");

    let html = StackList::new(
        layout(),
        &[healthy()],
        Some(&history),
        None,
        SystemTime::now(),
    )
    .render()
    .unwrap();
    assert!(html.contains("<dt>req/min</dt><dd>12</dd>"), "{html}");
    assert!(
        html.contains(r#"<dt>5xx</dt><dd class="stat-fail">2.0 %</dd>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="stack-spark-label">last hour</span>"#),
        "{html}"
    );
    assert!(html.contains(": 100 requests</span>"), "{html}");
    assert!(!html.contains("style="), "{html}");
}

#[test]
fn a_quiet_stack_shows_zeros_one_without_routes_none_and_an_unreadable_history_says_so() {
    let quiet = quiet();
    let card = StackCard::new(
        &healthy(),
        Condition::Running,
        Some(&quiet),
        SystemTime::now(),
    );
    let CardTraffic::Shown(traffic) = &card.traffic else {
        panic!("{:?}", card.traffic);
    };
    assert_eq!(traffic.per_minute.to_string(), "0");
    assert_eq!(traffic.line.p95, "-");
    assert_eq!(traffic.line.errors, "-");
    // A flat line along the bottom.
    let spark = traffic.spark.to_string();
    let heights: BTreeSet<&str> = spark
        .split(r#"points=""#)
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
        .split(' ')
        .map(|point| point.split(',').nth(1).unwrap())
        .collect();
    assert_eq!(heights.len(), 1, "{spark}");

    let unreadable = StackCard::new(&healthy(), Condition::Running, None, SystemTime::now());
    assert!(matches!(unreadable.traffic, CardTraffic::Unreadable));

    let mut unrouted = healthy();
    unrouted.revision.toml =
        "version = 1\nname = \"notes\"\n\n[services.api]\nimage = \"ghcr.io/acme/notes\"\n".into();
    for history in [Some(&quiet), None] {
        let card = StackCard::new(&unrouted, Condition::Running, history, SystemTime::now());
        assert!(card.routes.is_empty());
        assert!(matches!(card.traffic, CardTraffic::NoRoutes));
    }
    let html = StackList::new(layout(), &[unrouted], None, None, SystemTime::now())
        .render()
        .unwrap();
    assert!(html.contains("no routes, no traffic"), "{html}");
    let html = StackList::new(layout(), &[healthy()], None, None, SystemTime::now())
        .render()
        .unwrap();
    assert!(
        html.contains("the traffic history cannot be read"),
        "{html}"
    );
}

#[test]
fn ago_uses_the_largest_unit() {
    for (secs, text) in [
        (5, "just now"),
        (59, "just now"),
        (60, "1 min ago"),
        (3_599, "59 min ago"),
        (7_200, "2 h ago"),
        (86_400 * 12 + 5, "12 d ago"),
    ] {
        assert_eq!(Ago(Duration::from_secs(secs)).to_string(), text);
    }
}

#[tokio::test]
async fn the_list_filters_by_query_and_counts_the_stacks_traffic() {
    let app = TestApp::new().await;
    // Two minutes back is flushed whatever the second.
    let minute = Resolution::Minute.bucket(Timestamp::now()) - MINUTE * 2;
    app.stats.traffic.lock().unwrap().push(bucket(
        minute,
        ("notes.example.com", "notes", "api"),
        &[(200, 4, 12), (500, 6, 12)],
    ));
    let browser = app.sign_in().await;

    let html = body(app.get("/stacks", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<nav class="filters" aria-label="Status filter">"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<a class="filter" href="/stacks" aria-current="page">all <span class="filter-count">1</span></a>"#),
        "{html}"
    );
    // 10 requests in the last 5 minutes: 2 a minute.
    assert!(html.contains("<dt>req/min</dt><dd>2.0</dd>"), "{html}");
    assert!(
        html.contains(r#"<dt>5xx</dt><dd class="stat-fail">60.0 %</dd>"#),
        "{html}"
    );

    // A history that cannot be read leaves the cards without figures; the page still serves.
    app.stats
        .broken
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let response = app.get("/stacks", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("the traffic history cannot be read"),
        "{html}"
    );
    app.stats
        .broken
        .store(false, std::sync::atomic::Ordering::Relaxed);

    let html = body(
        app.get("/stacks?status=failed", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(
        html.contains(r#"<h2><a href="/stacks/notes">notes</a></h2>"#),
        "{html}"
    );
    let html = body(
        app.get("/stacks?status=running", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(!html.contains(r#"<h2><a href="/stacks/notes">"#), "{html}");
    // An unknown status shows every stack.
    let response = app.get("/stacks?status=bogus", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body(response)
            .await
            .contains(r#"<h2><a href="/stacks/notes">"#)
    );
}
