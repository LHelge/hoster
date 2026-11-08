//! The dashboard's overview: its warnings and table rendered on their own, and the page and its
//! live stream through the whole app against the fake orchestrator.

use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::StatusCode;
use http_body_util::BodyExt as _;

use super::*;
use crate::health::HealthStats;
use crate::orchestrator::RestartStats;
use crate::routing::{RouteEntry, RouteOwner, Target};
use crate::stack::RouteTimeouts;
use crate::stats::{RequestOutcome, Traffic};
use crate::store::{
    ContentHash, Deployment, DeploymentId, DeploymentOutcome, DeploymentSource, DeploymentState,
    DigestSet, RevisionId, StackRevision, Timestamp,
};
use crate::web::auth::tests::TestApp;
use crate::web::stacks::tests::body;

const BLOG: &str = r#"version = 1
name = "blog"

[services.web]
image = "ghcr.io/acme/blog"

[[services.web.routes]]
host = "blog.example.com"
port = 8080

[[services.web.routes]]
host = "www.blog.example.com"
port = 8080

[services.worker]
image = "ghcr.io/acme/blog-worker"
"#;

const DAY: Duration = Duration::from_secs(86_400);

fn digest(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

fn deployment(id: i64, state: DeploymentState) -> Deployment {
    Deployment {
        id: DeploymentId::from(id),
        stack: "blog".into(),
        revision: Some(RevisionId::from(1)),
        generation: Some(4),
        state,
        source: DeploymentSource::Webhook,
        target: DigestSet::from([("web".into(), digest(1))]),
        provenance: None,
        error: None,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        committed_at: None,
        finished_at: state.is_terminal().then(Timestamp::now),
    }
}

fn service(name: &str, health: HealthStatus, restarts: u32, crash_looping: bool) -> ServiceStatus {
    ServiceStatus {
        name: name.into(),
        image: format!("ghcr.io/acme/{name}"),
        ci_built: true,
        state: ServiceState::Running {
            generation: 4,
            digest: digest(1),
        },
        restarts: RestartStats {
            restarts,
            crash_looping,
        },
        health: HealthStats {
            status: health,
            last: Some(format!("GET /healthz of <{name}>: 503")),
            ..HealthStats::default()
        },
    }
}

/// `blog`: web healthy, worker crash looping, latest deployment live.
fn blog() -> StackStatus {
    StackStatus {
        name: "blog".into(),
        revision: StackRevision {
            id: RevisionId::from(1),
            stack: "blog".into(),
            revision: 1,
            toml: BLOG.into(),
            hash: ContentHash::of(BLOG),
            created_at: Timestamp::now(),
        },
        services: vec![
            service("web", HealthStatus::Healthy, 0, false),
            service("worker", HealthStatus::Unhealthy, 6, true),
        ],
        active: None,
        queued: None,
        last: Some(deployment(7, DeploymentState::Live)),
        flag: None,
        rejections: Vec::new(),
    }
}

fn cert(host: &str, left: Duration, now: SystemTime) -> CertExpiry {
    CertExpiry {
        host: host.into(),
        not_after: now + left,
    }
}

fn kinds(overview: &Overview) -> Vec<(WarningKind, &str)> {
    overview
        .warnings
        .iter()
        .map(|w| (w.kind, w.subject.as_str()))
        .collect()
}

fn compact(html: &str) -> String {
    html.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The overview of `stacks` with the `served` certificates, and nothing else.
fn overview(stacks: Vec<StackStatus>, served: Vec<CertExpiry>, now: SystemTime) -> Overview {
    Overview::new(
        Readings {
            stacks: Ok(stacks),
            served,
            ..Readings::default()
        },
        now,
    )
}

fn entry(owner: Owner, prefix: &str) -> RouteEntry {
    RouteEntry {
        owner,
        prefix: prefix.parse().unwrap(),
        prefix_str: prefix.to_owned(),
        strip_prefix: false,
        target: Target::Unavailable {
            retry_after: Duration::from_secs(1),
        },
        timeouts: RouteTimeouts::default(),
    }
}

fn routed(stack: &str, service: &str) -> Owner {
    Owner::Route(RouteOwner {
        stack: stack.parse().unwrap(),
        service: service.parse().unwrap(),
    })
}

fn finish(request: &mut crate::stats::TrackedRequest, status: u16, millis: u64) {
    request.finish(RequestOutcome {
        status: Some(status),
        latency: Duration::from_millis(millis),
        upstream_error: false,
    });
}

/// The `<tr>` of `stack` in the stacks table.
fn row<'a>(html: &'a str, stack: &str) -> &'a str {
    let start = html
        .find(&format!(r#"<a class="dash-stack" href="/stacks/{stack}">"#))
        .unwrap_or_else(|| panic!("no row of {stack}: {html}"));
    let end = html[start..].find("</tr>").unwrap() + start;
    &html[start..end]
}

#[test]
fn a_healthy_stack_raises_no_warnings() {
    let now = SystemTime::now();
    let mut stack = blog();
    stack.services[1] = service("worker", HealthStatus::Healthy, 1, false);
    let served = vec![
        cert("blog.example.com", 60 * DAY, now),
        cert("www.blog.example.com", 30 * DAY, now),
    ];
    let overview = overview(vec![stack], served, now);
    assert!(overview.warnings.is_empty(), "{:?}", overview.warnings);

    let html = compact(&overview.render().unwrap());
    assert!(
        !html.contains("warnings-title"),
        "no alert without warnings: {html}"
    );
    let row = row(&html, "blog");
    assert!(
        row.contains(&format!(
            r#"<td><span class="pill pill-ok">{}running</span></td>"#,
            crate::web::Icon::Ok
        )),
        "{row}"
    );
    assert!(
        row.contains(
            r#"<span class="stat-neutral" title="web: healthy, worker: healthy">2/2</span>"#
        ),
        "{row}"
    );
    assert!(
        row.contains(r#"<span class="stat-neutral" title="web: 0, worker: 1">1</span>"#),
        "{row}"
    );
    assert!(
        row.contains(&format!(
            r#"<a href="/stacks/blog/deployments/7">#7</a> <span class="pill pill-ok">{}live</span> <time class="muted dash-ago""#,
            crate::web::Icon::Ok
        )),
        "{row}"
    );
    assert!(
        row.contains(r#"<a href="/stacks/blog/deployments">history"#),
        "{row}"
    );
    // blog routes hosts but has seen no requests yet.
    assert!(row.contains(r#"<td class="num">-</td>"#), "{row}");
    assert!(!html.contains("pill-fail"), "{html}");
    assert!(!html.contains("pill-warn"), "{html}");
    assert!(
        html.contains("Stacks <span class=\"card-count\">· 1 running</span>"),
        "{html}"
    );
}

#[test]
fn every_warning_kind_is_raised_and_badged() {
    let now = SystemTime::now();
    let mut blog = blog();
    blog.services[0] = service("web", HealthStatus::Unhealthy, 0, false);
    let mut failed = deployment(8, DeploymentState::Failed);
    failed.error = Some("health check of <web> timed out".into());
    blog.last = Some(failed);
    let served = vec![
        cert("admin.example.com", 2 * DAY, now),
        cert("blog.example.com", 13 * DAY, now),
        cert("www.blog.example.com", 15 * DAY, now),
    ];
    let overview = overview(vec![blog], served, now);
    assert_eq!(
        kinds(&overview),
        [
            (WarningKind::Unhealthy, "web (blog)"),
            (WarningKind::CrashLooping, "worker (blog)"),
            (WarningKind::DeployFailed, "blog"),
            (WarningKind::CertExpiring, "admin.example.com"),
            (WarningKind::CertExpiring, "blog.example.com"),
        ]
    );
    let deploy = &overview.warnings[2];
    assert_eq!(
        deploy.detail,
        "deployment 8 Failed: health check of <web> timed out"
    );
    assert_eq!(deploy.href, "/stacks/blog/deployments/8");
    // No stack routes it (hoster's own host here): nobody is named.
    assert_eq!(overview.warnings[3].detail, "2 d left");
    assert_eq!(overview.warnings[4].detail, "13 d left, used by web (blog)");
    assert_eq!(overview.warnings[4].href, "/certificates");

    let html = compact(&overview.render().unwrap());
    assert!(
        html.contains(r#"<section class="alert alert-fail" aria-labelledby="warnings-title">"#),
        "{html}"
    );
    assert!(html.contains(">5 warnings</h2>"), "{html}");
    assert!(
        html.contains(
            &format!(r#"<li><span class="pill pill-fail">{}failing health check</span> <a href="/stacks/blog">web (blog)</a><span class="warning-detail">GET /healthz of &#60;web&#62;: 503</span></li>"#, crate::web::Icon::Failed)
        ),
        "{html}"
    );
    assert!(
        html.contains(&format!(r#"<span class="pill pill-fail">{}crash looping</span> <a href="/stacks/blog">worker (blog)</a>"#, crate::web::Icon::Failed)),
        "{html}"
    );
    assert!(
        html.contains(
            &format!(r#"<span class="pill pill-fail">{}deploy failed</span> <a href="/stacks/blog/deployments/8">blog</a><span class="warning-detail">deployment 8 Failed: health check of &#60;web&#62; timed out</span>"#, crate::web::Icon::Failed)
        ),
        "the detail is escaped: {html}"
    );
    assert!(
        html.contains(&format!(r#"<span class="pill pill-warn">{}certificate expiring</span> <a href="/certificates">blog.example.com</a>"#, crate::web::Icon::Warning)),
        "{html}"
    );
    // In the table: the failed condition and deployment, the crash loop, no healthy service.
    let row = row(&html, "blog");
    assert!(
        row.contains(&format!(
            r#"<td><span class="pill pill-fail">{}failed</span></td>"#,
            crate::web::Icon::Failed
        )),
        "{row}"
    );
    assert!(
        row.contains(&format!(
            r#"#8</a> <span class="pill pill-fail">{}failed</span>"#,
            crate::web::Icon::Failed
        )),
        "{row}"
    );
    assert!(
        row.contains(
            r#"<span class="stat-warn" title="web: unhealthy, worker: crash looping">0/2</span>"#
        ),
        "{row}"
    );
    assert!(
        row.contains(r#"<span class="stat-fail" title="web: 0, worker: 6">6</span><span class="dash-sub stat-fail">1 crash looping</span>"#),
        "{row}"
    );
}

#[test]
fn only_certificate_warnings_tint_the_alert_amber() {
    let now = SystemTime::now();
    let mut stack = blog();
    stack.services[1] = service("worker", HealthStatus::Healthy, 0, false);
    let overview = overview(
        vec![stack],
        vec![cert("blog.example.com", 3 * DAY, now)],
        now,
    );
    let html = compact(&overview.render().unwrap());
    assert!(
        html.contains(r#"<section class="alert alert-warn""#),
        "{html}"
    );
    assert!(html.contains(">1 warning</h2>"), "{html}");
}

#[test]
fn a_crash_loop_reported_by_the_health_monitor_counts_too() {
    let now = SystemTime::now();
    let mut blog = blog();
    blog.services[1] = service("worker", HealthStatus::CrashLooping, 0, false);
    let overview = overview(vec![blog], Vec::new(), now);
    assert_eq!(
        kinds(&overview),
        [(WarningKind::CrashLooping, "worker (blog)")]
    );
}

#[test]
fn a_deploying_stack_shows_its_running_deployment_and_the_queue() {
    let now = SystemTime::now();
    let mut blog = blog();
    blog.services[0].state = ServiceState::AwaitingFirstDigest;
    blog.services[0].health = HealthStats::default();
    // A crash-loop flag left from an old generation means nothing without one running.
    blog.services[1].state = ServiceState::NotDeployed;
    blog.active = Some((DeploymentId::from(9), DeploymentState::Pulling));
    blog.queued = Some(DeploymentId::from(10));
    let overview = overview(vec![blog], Vec::new(), now);
    assert!(overview.warnings.is_empty(), "{:?}", overview.warnings);
    let html = compact(&overview.render().unwrap());
    let row = row(&html, "blog");
    assert!(
        row.contains(&format!(
            r#"<td><span class="pill pill-progress">{}deploying</span></td>"#,
            crate::web::Icon::Pending
        )),
        "{row}"
    );
    assert!(
        row.contains(r#"title="web: awaiting its first digest, worker: not deployed">0/2</span>"#),
        "{row}"
    );
    assert!(row.contains(r#"title="No service runs">0</span>"#), "{row}");
    assert!(
        row.contains(&format!(r#"<a href="/stacks/blog/deployments/9">#9</a> <span class="pill pill-progress">{}pulling</span>"#, crate::web::Icon::Pending)),
        "{row}"
    );
    assert!(row.contains("#10 queued · <a"), "{row}");
}

#[test]
fn a_stack_whose_revision_cannot_be_read_still_shows() {
    let now = SystemTime::now();
    let mut blog = blog();
    blog.revision.toml = "not a stack file".into();
    // The orchestrator reports no services for a revision it cannot parse.
    blog.services.clear();
    let mut failed = deployment(8, DeploymentState::Failed);
    failed.error = Some("boom".into());
    blog.last = Some(failed);
    blog.flag = Some("revision 1: deploy failed: boom".into());
    let overview = overview(
        vec![blog],
        vec![cert("blog.example.com", 2 * DAY, now)],
        now,
    );
    assert_eq!(
        kinds(&overview),
        [
            (WarningKind::DeployFailed, "blog"),
            (WarningKind::CertExpiring, "blog.example.com"),
        ]
    );
    let html = compact(&overview.render().unwrap());
    let row = row(&html, "blog");
    assert!(
        row.contains(&format!(r#"<span class="dash-flag"><span class="pill pill-fail pill-long">{}revision 1: deploy failed: boom</span></span>"#, crate::web::Icon::Failed)),
        "{row}"
    );
    assert!(
        row.contains(
            r#"<td class="num muted" colspan="4">the active revision cannot be read</td>"#
        ),
        "{row}"
    );
    assert!(!row.contains("no routes"), "{row}");
    assert!(row.contains(">0/0</span>"), "{row}");
}

#[test]
fn the_stacks_table_aggregates_each_stacks_routes_and_deployments() {
    let now = SystemTime::now();
    let traffic = Traffic::default();
    let root = entry(routed("blog", "web"), "/");
    let api = entry(routed("blog", "web"), "/api");
    for _ in 0..90 {
        finish(&mut traffic.route("blog.example.com", &root), 200, 4);
    }
    for status in [404, 404, 503, 500, 200, 200, 200, 200, 200, 200] {
        finish(&mut traffic.route("blog.example.com", &api), status, 1_500);
    }
    let removed = entry(routed("gone", "web"), "/");
    finish(&mut traffic.route("gone.example.com", &removed), 200, 2);
    let admin = entry(Owner::Reserved("admin UI"), "/");
    for _ in 0..3 {
        finish(&mut traffic.route("admin.example.com", &admin), 200, 2);
    }
    finish(&mut traffic.unrouted(), 404, 1);
    let outcome = |state, count, mean, max| DeploymentOutcome {
        stack: "blog".into(),
        state,
        count,
        mean: Duration::from_secs(mean),
        max: Duration::from_secs(max),
    };
    let deployments = vec![DeployStats {
        stack: "blog".into(),
        outcomes: vec![
            outcome(DeploymentState::Live, 7, 12, 40),
            outcome(DeploymentState::Failed, 2, 5, 9),
            outcome(DeploymentState::Noop, 3, 1, 1),
        ],
    }];
    let overview = Overview::new(
        Readings {
            stacks: Ok(vec![blog()]),
            traffic: traffic.snapshot(),
            deployments: Ok(deployments),
            ..Readings::default()
        },
        now,
    );
    let html = compact(&overview.render().unwrap());
    let row = row(&html, "blog");
    // 100 requests over both routes: p50 is the fast route's, p95 the slow one's.
    assert!(
        row.contains(r#"<td class="num">4.1 ms</td> <td class="num stat-warn">1.6 s</td>"#),
        "{row}"
    );
    assert!(
        row.contains(r#"<td class="num">2.0 %</td> <td class="num stat-fail">2.0 %</td>"#),
        "4xx and 5xx shares: {row}"
    );
    assert!(
        row.contains(r#"7 live · <span class="stat-fail">2 failed</span> · 3 skipped</span><span class="dash-sub">mean 12.0 s · max 40.0 s</span>"#),
        "{row}"
    );
    assert!(
        row.contains("Finished deployments of the last 30 d: 7 went live"),
        "{row}"
    );
    // What no stack row counts.
    assert!(
        html.contains(r#"Outside the stacks: <span class="mono">unrouted</span> <span title="Responses since hoster started: none 0 · 1xx 0 · 2xx 0 · 3xx 0 · 4xx 1 · 5xx 0">1 request (4xx 100.0 %, 5xx 0 %"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="mono">gone (removed)</span> <span title="Responses since hoster started: none 0 · 1xx 0 · 2xx 1 · 3xx 0 · 4xx 0 · 5xx 0">1 request"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="mono">hoster (admin UI)</span> <span title="Responses since hoster started: none 0 · 1xx 0 · 2xx 3 · 3xx 0 · 4xx 0 · 5xx 0">3 requests"#),
        "{html}"
    );

    // The tiles count everything: 105 requests, two 5xx.
    let tiles: Vec<String> = overview.kpis.iter().map(ToString::to_string).collect();
    assert!(
        tiles[0].contains(r#"<dd class="kpi-value">105</dd>"#),
        "{tiles:?}"
    );
    assert!(
        tiles[1].contains(
            r#"<dd class="kpi-value">1.9 %</dd><dd class="kpi-sub">2 of 105 since start</dd>"#
        ),
        "{tiles:?}"
    );
    assert!(tiles[1].contains("kpi-fail"), "{tiles:?}");
    assert!(
        tiles[2].contains("p50 latency") && tiles[2].contains("last 5 min"),
        "{tiles:?}"
    );
    // blog's worker crash loops: degraded.
    assert!(
        tiles[4].contains(r#"<div class="kpi kpi-warn""#)
            && tiles[4].contains(r#"<dd class="kpi-value">0 / 1</dd><dd class="kpi-sub">1 degraded · 0 deploying</dd>"#),
        "{tiles:?}"
    );
}

#[test]
fn unreadable_deployment_statistics_are_named_and_leave_the_rest() {
    let overview = Overview::new(
        Readings {
            stacks: Ok(vec![blog()]),
            deployments: Err("Cannot show the deployment statistics: database is locked.".into()),
            ..Readings::default()
        },
        SystemTime::now(),
    );
    let html = compact(&overview.render().unwrap());
    assert!(
        html.contains(r#"<p class="flash flash-error">Cannot show the deployment statistics: database is locked.</p>"#),
        "{html}"
    );
    assert!(
        row(&html, "blog")
            .contains(r#"<td class="dash-deploys"><span class="muted">-</span></td>"#),
        "{html}"
    );
}

#[test]
fn an_empty_overview_says_so() {
    let html = overview(Vec::new(), Vec::new(), SystemTime::now())
        .render()
        .unwrap();
    assert!(!html.contains("warnings-title"));
    assert!(html.contains("No stacks yet."));
    assert!(html.contains("No hosts need a certificate yet."));
    assert!(html.contains(r#"<dd class="kpi-value">0 / 0</dd>"#));
}

#[test]
fn a_failing_orchestrator_is_named_above_the_overview() {
    let traffic = Traffic::default();
    let root = entry(routed("blog", "web"), "/");
    finish(&mut traffic.route("blog.example.com", &root), 200, 4);
    let overview = Overview::new(
        Readings {
            stacks: Err("Cannot show the stacks: the store is locked.".into()),
            traffic: traffic.snapshot(),
            ..Readings::default()
        },
        SystemTime::now(),
    );
    let html = compact(&overview.render().unwrap());
    assert!(
        html.contains(
            r#"<p class="flash flash-error">Cannot show the stacks: the store is locked.</p>"#
        ),
        "{html}"
    );
    // Which stacks exist is not known: none is called removed, or missing.
    assert!(!html.contains("(removed)"), "{html}");
    assert!(
        html.contains(r#"<span class="mono">blog</span> <span title="#),
        "{html}"
    );
    assert!(!html.contains("No stacks yet."), "{html}");
    assert!(
        html.contains("The stacks cannot be read right now."),
        "{html}"
    );
    let tiles: Vec<String> = overview.kpis.iter().map(ToString::to_string).collect();
    assert!(
        tiles[4].contains("kpi-fail")
            && tiles[4]
                .contains(r#"<dd class="kpi-value">-</dd><dd class="kpi-sub">cannot be read</dd>"#),
        "{tiles:?}"
    );
}

#[test]
fn removed_stacks_keep_their_upstream_errors_and_deployments() {
    let traffic = Traffic::default();
    let removed = entry(routed("gone", "web"), "/");
    let mut request = traffic.route("gone.example.com", &removed);
    request.finish(RequestOutcome {
        status: Some(502),
        latency: Duration::from_millis(3),
        upstream_error: true,
    });
    finish(&mut traffic.route("gone.example.com", &removed), 200, 2);
    let outcome = |stack: &str, state, count| DeploymentOutcome {
        stack: stack.into(),
        state,
        count,
        mean: Duration::from_secs(10),
        max: Duration::from_secs(20),
    };
    let deployments = vec![
        DeployStats {
            stack: "gone".into(),
            outcomes: vec![
                outcome("gone", DeploymentState::Live, 2),
                outcome("gone", DeploymentState::Failed, 1),
            ],
        },
        DeployStats {
            stack: "old".into(),
            outcomes: vec![outcome("old", DeploymentState::Failed, 1)],
        },
    ];
    let overview = Overview::new(
        Readings {
            stacks: Ok(vec![blog()]),
            traffic: traffic.snapshot(),
            deployments: Ok(deployments),
            ..Readings::default()
        },
        SystemTime::now(),
    );
    let html = compact(&overview.render().unwrap());
    assert!(
        html.contains(r#"<span class="mono">gone (removed)</span> <span title="Responses since hoster started: none 0 · 1xx 0 · 2xx 1 · 3xx 0 · 4xx 0 · 5xx 1">2 requests"#),
        "{html}"
    );
    assert!(
        html.contains(r#", <span class="stat-fail">1 upstream error</span>)"#),
        "{html}"
    );
    assert!(
        html.contains(r#"deploys · 30 d: 2 live · <span class="stat-fail">1 failed</span>, mean 10.0 s · max 20.0 s</span>"#),
        "{html}"
    );
    // A removed stack with deployments but no requests shows only those.
    assert!(
        html.contains(
            r#"<span class="mono">old (removed)</span> <span title="Finished deployments"#
        ),
        "{html}"
    );
}

#[tokio::test]
async fn bursts_of_events_render_once() {
    let (sender, mut events) = broadcast::channel(16);
    for id in 1..=3 {
        sender
            .send(UiEvent::Deployment {
                stack: "blog".into(),
                id: DeploymentId::from(id),
                state: DeploymentState::Pulling,
            })
            .unwrap();
    }
    assert!(OverviewEvents::changed(&mut events).await);
    assert!(events.try_recv().is_err(), "the burst was taken whole");
    drop(sender);
    assert!(!OverviewEvents::changed(&mut events).await);
}

#[tokio::test(start_paused = true)]
async fn a_quiet_stream_renders_again_after_the_refresh_interval() {
    let (_sender, mut events) = broadcast::channel::<UiEvent>(16);
    let started = tokio::time::Instant::now();
    assert!(OverviewEvents::changed(&mut events).await);
    assert_eq!(started.elapsed(), OverviewEvents::REFRESH);
}

#[tokio::test]
async fn the_dashboard_shows_the_overview_in_a_live_region() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    *app.stacks.stacks.lock().unwrap() = vec![blog()];
    app.stats.certificates.lock().unwrap().push(CertExpiry {
        host: "blog.example.com".into(),
        not_after: SystemTime::now() + 3 * DAY + Duration::from_secs(60),
    });
    let now = SystemTime::now();
    *app.certificates.overview.lock().unwrap() = Some(crate::web::certificates::CertOverview {
        source: "the A/AAAA records of admin.example.com".into(),
        public: Default::default(),
        hosts: vec![crate::web::certificates::CertRow {
            host: "blog.example.com".into(),
            issuer: Some("Let's Encrypt R11".into()),
            validity: Some(crate::acme::Validity {
                not_before: now - 87 * DAY,
                not_after: now + 3 * DAY + Duration::from_secs(60),
            }),
            status: Default::default(),
            backoff: None,
            owner: crate::web::certificates::CertOwner::Stacks(vec!["blog".into()]),
        }],
        directory: crate::web::certificates::AcmeDirectory::LetsEncrypt,
    });

    let html = body(app.get("/", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(
            r#"<div id="overview" class="dashboard" hx-sse:connect="/events" hx-swap="innerHTML" hx-sse:close="end">"#
        ),
        "{html}"
    );
    let html = compact(&html);
    assert!(
        html.contains(r#"<a href="/certificates">blog.example.com</a><span class="warning-detail">3 d left, used by web (blog)</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/stacks/blog">worker (blog)</a>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<a class="dash-stack" href="/stacks/blog">blog</a>"#),
        "{html}"
    );
    // The certificates card, from the certificates page's control.
    assert!(
        html.contains(r#"<code class="glance-host">blog.example.com</code>"#),
        "{html}"
    );
    assert!(html.contains(">3d left</time>"), "{html}");
    assert!(
        html.contains(
            r#"<dd class="kpi-value">0 / 1</dd><dd class="kpi-sub">0 failing · 1 expiring</dd>"#
        ),
        "{html}"
    );
    assert!(!html.contains("style="), "{html}");
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
async fn the_stream_sends_the_overview_again_on_every_change() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    *app.stacks.stacks.lock().unwrap() = vec![blog()];

    let response = app.get("/events", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut stream = response.into_body();
    let mut text = String::new();
    read_until(&mut stream, &mut text, "\n\n").await;
    assert!(
        text.starts_with("data: <section aria-label=\"Key figures\">"),
        "{text}"
    );
    assert!(text.contains("worker (blog)"), "{text}");
    assert!(!text.contains("failing health check"), "{text}");

    // web's probe starts failing.
    app.stacks.stacks.lock().unwrap()[0].services[0] =
        service("web", HealthStatus::Unhealthy, 0, false);
    app.stacks
        .events
        .send(UiEvent::Health {
            stack: "blog".into(),
            service: "web".into(),
            generation: 4,
            from: HealthStatus::Healthy,
            to: HealthStatus::Unhealthy,
            detail: "503".into(),
        })
        .unwrap();
    text.clear();
    read_until(&mut stream, &mut text, "\n\n").await;
    assert!(text.contains("failing health check"), "{text}");
    assert!(!text.contains("event: end"), "{text}");

    // Signing out ends the stream.
    let logout = axum::http::Request::post("/auth/logout")
        .header("cookie", &browser.cookie)
        .header("x-csrf-token", &browser.csrf)
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    app.send(logout).await;
    text.clear();
    read_until(&mut stream, &mut text, "event: end").await;
    // The stale overview is replaced by a notice before the stream ends.
    assert!(
        text.starts_with(&format!("data: {}", OverviewEvents::SIGNED_OUT)),
        "{text}"
    );
}

#[tokio::test]
async fn the_stream_needs_a_session() {
    let app = TestApp::new().await;
    let response = app.get("/events", None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}
