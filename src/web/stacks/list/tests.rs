//! The stacks list: conditions, the filter, the cards and their statistics.

use std::time::Duration;

use axum::http::StatusCode;

use super::*;
use crate::health::HealthStatus;
use crate::routing::{Owner, RouteEntry, RouteOwner, Target};
use crate::stack::RouteTimeouts;
use crate::stats::{RequestOutcome, Traffic};
use crate::store::{DeploymentId, Provenance};
use crate::web::auth::Session;
use crate::web::auth::tests::TestApp;
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

fn entry(stack: &str, prefix: &str) -> RouteEntry {
    RouteEntry {
        owner: Owner::Route(RouteOwner {
            stack: stack.parse().unwrap(),
            service: "api".parse().unwrap(),
        }),
        prefix: prefix.parse().unwrap(),
        prefix_str: prefix.to_owned(),
        strip_prefix: false,
        target: Target::Unavailable {
            retry_after: Duration::from_secs(1),
        },
        timeouts: RouteTimeouts::default(),
    }
}

fn request(traffic: &Traffic, host: &str, route: &RouteEntry, status: u16, millis: u64) {
    traffic.route(host, route).finish(RequestOutcome {
        status: Some(status),
        latency: Duration::from_millis(millis),
        upstream_error: false,
    });
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
    let traffic = TrafficSnapshot::default();

    let all = StackList::new(layout(), &stacks, &traffic, None, SystemTime::now());
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
        &traffic,
        Some(Condition::Degraded),
        SystemTime::now(),
    );
    assert!(degraded.filters[2].current && !degraded.filters[0].current);
    let names: Vec<&str> = degraded.cards.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["blog"]);

    let html = StackList::new(
        layout(),
        &stacks,
        &traffic,
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
        &traffic,
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
    let html = StackList::new(
        layout(),
        &[],
        &TrafficSnapshot::default(),
        None,
        SystemTime::now(),
    )
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
    let card = StackCard::new(
        &status,
        Condition::Running,
        &TrafficSnapshot::default(),
        now,
    );
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
    let card = StackCard::new(
        &status,
        Condition::Deploying,
        &TrafficSnapshot::default(),
        now,
    );
    assert!(card.last.is_none());
    assert_eq!(
        card.active,
        Some((
            "3".into(),
            "Pulling".into(),
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
        &TrafficSnapshot::default(),
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
        &TrafficSnapshot::default(),
        SystemTime::now(),
    );
    assert_eq!(card.services[1].shown_word(), Some("waiting"));
    assert_eq!(card.services[1].spoken(), "awaiting its first digest");
    assert_eq!(card.services[1].tone, Tone::Neutral);
}

#[test]
fn traffic_adds_up_the_stacks_routes_and_takes_the_slowest_p95() {
    let traffic = Traffic::default();
    let root = entry("notes", "/");
    let api = entry("notes", "/api");
    let other = entry("blog", "/");
    for _ in 0..97 {
        request(&traffic, "notes.example.com", &root, 200, 5);
    }
    request(&traffic, "notes.example.com", &api, 503, 2_000);
    request(&traffic, "notes.example.com", &api, 502, 2_000);
    request(&traffic, "notes.example.com", &api, 200, 2_000);
    for _ in 0..50 {
        request(&traffic, "blog.example.com", &other, 500, 9_000);
    }
    let rare = entry("shop", "/");
    for _ in 0..4_999 {
        request(&traffic, "shop.example.com", &rare, 200, 5);
    }
    request(&traffic, "shop.example.com", &rare, 500, 5);
    let snapshot = traffic.snapshot();
    let line = TrafficLine::of(
        snapshot
            .routes
            .iter()
            .filter(|r| r.stack() == Some("notes")),
    );
    assert_eq!(line.requests, 100);
    assert_eq!(line.errors, "2.0 %");
    assert_eq!(line.errors_tone, Tone::Fail);
    assert_eq!(line.p95_tone, Tone::Warn, "{}", line.p95);

    let quiet = TrafficLine::of(std::iter::empty());
    assert_eq!((quiet.requests, quiet.p95.as_str()), (0, "-"));
    assert_eq!(
        (quiet.errors.as_str(), quiet.errors_tone),
        ("-", Tone::Neutral)
    );

    let fine = TrafficLine::of(
        snapshot
            .routes
            .iter()
            .filter(|r| r.stack() == Some("notes") && r.prefix == "/"),
    );
    assert_eq!(
        (fine.errors.as_str(), fine.p95_tone),
        ("0 %", Tone::Neutral)
    );

    // One error in 5000 rounds to 0.0 but is not none.
    let rare = TrafficLine::of(snapshot.routes.iter().filter(|r| r.stack() == Some("shop")));
    assert_eq!(
        (rare.errors.as_str(), rare.errors_tone),
        ("<0.1 %", Tone::Neutral)
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
    let route = entry("notes", "/");
    request(&app.traffic, "notes.example.com", &route, 200, 12);
    request(&app.traffic, "notes.example.com", &route, 500, 12);
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
    assert!(html.contains("<dt>requests</dt><dd>2</dd>"), "{html}");
    assert!(
        html.contains(r#"<dt>5xx</dt><dd class="stat-fail">50.0 %</dd>"#),
        "{html}"
    );

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
