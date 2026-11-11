//! The dashboard's overview: its warnings and table rendered on their own, and the page and its
//! live stream through the whole app against the fake orchestrator.

use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::StatusCode;
use http_body_util::BodyExt as _;

use super::*;
use crate::health::HealthStats;
use crate::orchestrator::RestartStats;
use crate::routing::{Owner, RouteEntry, RouteOwner, Target};
use crate::stack::RouteTimeouts;
use crate::stats::tests::FakeStats;
use crate::stats::{Histogram, RequestOutcome, Traffic};
use crate::store::{
    ContentHash, Deployment, DeploymentId, DeploymentOutcome, DeploymentSource, DeploymentState,
    DigestSet, EventId, EventKind, RevisionId, Severity, StackRevision, TrafficBucket,
    TrafficCounts, TrafficSeries,
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
            // A crash loop names its last exit (ADR-0059 §6).
            last_exit: crash_looping.then_some(crate::orchestrator::ExitReason::OomKilled(
                crate::orchestrator::MemoryLimit(Some(crate::stack::ByteSize::mebibytes(256))),
            )),
        },
        health: HealthStats {
            status: health,
            last: Some(format!("GET /healthz of <{name}>: 503")),
            ..HealthStats::default()
        },
        waiting_for: Vec::new(),
    }
}

/// `blog`: web healthy, worker crash looping, latest deployment live.
fn blog() -> StackStatus {
    StackStatus {
        name: "blog".into(),
        committed: None,
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

pub(in crate::web) const MINUTE: Duration = Duration::from_secs(60);

/// The start of an hour; [`now`] is 30 min 30 s into it.
pub(in crate::web) fn hour() -> Timestamp {
    Timestamp::from(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_002_800))
}

/// 30 min 30 s into [`hour`]: the newest flushed minute is the hour's 30th (`hour + 29 min`),
/// the 24h range ends with the hour, and the rate window is `hour + 25 min` to `hour + 30 min`.
pub(in crate::web) fn now() -> Timestamp {
    hour() + MINUTE * 30 + Duration::from_secs(30)
}

/// A minute bucket at `start` of the route `host` owned by `stack`'s `service` (`""` for
/// none, `("", "admin UI")` for hoster's own), with `count` requests of each (status,
/// latency in ms); a 502 counts as an upstream error.
pub(in crate::web) fn bucket(
    start: Timestamp,
    (host, stack, service): (&str, &str, &str),
    requests: &[(u16, u64, u64)],
) -> TrafficBucket<Histogram> {
    let mut counts = TrafficCounts::default();
    let mut latency = Histogram::default();
    for &(status, count, millis) in requests {
        let class = match status {
            100..=199 => &mut counts.s1xx,
            200..=299 => &mut counts.s2xx,
            300..=399 => &mut counts.s3xx,
            400..=499 => &mut counts.s4xx,
            _ => &mut counts.s5xx,
        };
        *class += count;
        if status == 502 {
            counts.upstream_errors += count;
        }
        for _ in 0..count {
            latency.record(Duration::from_millis(millis));
        }
    }
    TrafficBucket {
        start,
        series: TrafficSeries {
            host: host.into(),
            prefix: if host.is_empty() { "" } else { "/" }.into(),
            stack: stack.into(),
            service: service.into(),
        },
        counts,
        latency,
    }
}

/// The traffic history over `range` as of [`now`] of the minute buckets `rows`.
pub(in crate::web) async fn history(
    range: Range,
    rows: Vec<TrafficBucket<Histogram>>,
) -> TrafficHistory {
    let fake = FakeStats::default();
    *fake.traffic.lock().unwrap() = rows;
    Stats::new(Traffic::default(), Arc::new(fake))
        .history_at(range, now())
        .await
        .unwrap()
}

/// The overview of `stacks` and `history`, and nothing else.
fn overview_of(stacks: Vec<StackStatus>, history: TrafficHistory) -> Overview {
    Overview::new(
        Readings {
            stacks: Ok(stacks),
            range: history.range(),
            history: Ok(history),
            ..Readings::default()
        },
        SystemTime::now(),
    )
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
        html.contains(&format!(r#"<span class="pill pill-fail">{}crash looping</span> <a href="/stacks/blog">worker (blog)</a><span class="warning-detail">last: OOM-killed (limit 256MiB) · GET /healthz of &#60;worker&#62;: 503</span>"#, crate::web::Icon::Failed)),
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
            r#"<td class="num muted" colspan="6">the active revision cannot be read</td>"#
        ),
        "{row}"
    );
    assert!(!row.contains("The stack routes nothing"), "{row}");
    assert!(row.contains(">0/0</span>"), "{row}");
}

#[tokio::test]
async fn the_stacks_table_aggregates_each_stacks_routes_and_deployments() {
    let now = SystemTime::now();
    let blog_web = ("blog.example.com", "blog", "web");
    let blog_api = ("api.blog.example.com", "blog", "web");
    let rows = vec![
        // In the rate window: 90 fast requests to one route, 10 slow ones to another.
        bucket(hour() + MINUTE * 27, blog_web, &[(200, 90, 4)]),
        bucket(
            hour() + MINUTE * 28,
            blog_api,
            &[
                (404, 2, 1_500),
                (503, 1, 1_500),
                (500, 1, 1_500),
                (200, 6, 1_500),
            ],
        ),
        // Earlier in the range, outside the rate window: 20 more.
        bucket(hour() - MINUTE * 100, blog_web, &[(200, 20, 4)]),
        bucket(
            hour() + MINUTE,
            ("gone.example.com", "gone", "web"),
            &[(200, 1, 2)],
        ),
        bucket(
            hour() + MINUTE,
            ("admin.example.com", "", "admin UI"),
            &[(200, 3, 2)],
        ),
        bucket(hour() + MINUTE, ("", "", ""), &[(404, 1, 1)]),
        // The period before the range: half as many requests, all faster.
        bucket(hour() - 25 * MINUTE * 60, blog_web, &[(200, 61, 1)]),
    ];
    let history = history(Range::Day, rows).await;
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
            range: Range::Day,
            history: Ok(history),
            inflight: 2,
            deployments: Ok(deployments),
            ..Readings::default()
        },
        now,
    );
    let html = compact(&overview.render().unwrap());
    let row = row(&html, "blog");
    // 100 requests in the last 5 minutes; 120 over both routes in the range: p50 is the fast
    // route's, p95 the slow one's.
    assert!(
        row.contains(r#"<td class="num">20</td> <td class="num">4.1 ms</td> <td class="num stat-warn">1.6 s</td>"#),
        "{row}"
    );
    assert!(
        row.contains(r#"<td class="num">1.7 %</td> <td class="num stat-fail">1.7 %</td>"#),
        "4xx and 5xx shares: {row}"
    );
    assert!(
        row.contains(r#"<td class="dash-spark"><span class="visually-hidden">120 requests</span><svg class="spark spark-warn""#),
        "the sparkline is in the tone of the stack's condition (degraded): {row}"
    );
    assert!(
        row.contains(r#"7 live · <span class="stat-fail">2 failed</span> · 3 skipped</span><span class="dash-sub">mean 12.0 s · max 40.0 s</span>"#),
        "{row}"
    );
    assert!(
        row.contains("Finished deployments of the last 30 d: 7 went live"),
        "{row}"
    );
    // What no stack row counts, over the range.
    assert!(
        html.contains(r#"Outside the stacks: <span class="mono">unrouted</span> <span title="Responses over the last 24h: none 0 · 1xx 0 · 2xx 0 · 3xx 0 · 4xx 1 · 5xx 0">1 request (4xx 100.0 %, 5xx 0 %"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="mono">gone (removed)</span> <span title="Responses over the last 24h: none 0 · 1xx 0 · 2xx 1 · 3xx 0 · 4xx 0 · 5xx 0">1 request"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="mono">hoster (admin UI)</span> <span title="Responses over the last 24h: none 0 · 1xx 0 · 2xx 3 · 3xx 0 · 4xx 0 · 5xx 0">3 requests"#),
        "{html}"
    );

    // The tiles count everything but hoster's own admin UI: 122 requests, two 5xx; the period
    // before had 61.
    let tiles: Vec<String> = overview.kpis.iter().map(ToString::to_string).collect();
    assert!(
        tiles[0].contains(r#"<dt class="kpi-label">requests · 24h</dt><dd class="kpi-value">122</dd><dd class="kpi-sub">▲ 100.0 % vs prev · 2 in flight</dd>"#),
        "{tiles:?}"
    );
    assert!(
        tiles[1].contains(r#"<dd class="kpi-value">1.6 %</dd><dd class="kpi-sub">2 of 122</dd>"#),
        "{tiles:?}"
    );
    assert!(tiles[1].contains("kpi-fail"), "{tiles:?}");
    assert!(
        tiles[2].contains(
            r#"<dd class="kpi-value">4.1 ms</dd><dd class="kpi-sub">▲ 3.1 ms vs prev</dd>"#
        ),
        "{tiles:?}"
    );
    assert!(
        tiles[3].contains("kpi-warn") && tiles[3].contains("vs prev"),
        "{tiles:?}"
    );
    assert!(
        tiles[0].contains("over the last 24h") && !tiles.concat().contains("since"),
        "{tiles:?}"
    );
    // blog's worker crash loops: degraded.
    assert!(
        tiles[4].contains(r#"<div class="kpi kpi-warn""#)
            && tiles[4].contains(r#"<dd class="kpi-value">0 / 1</dd><dd class="kpi-sub">1 degraded · 0 deploying</dd>"#),
        "{tiles:?}"
    );
    assert!(!html.contains("since hoster started"), "{html}");
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

#[tokio::test]
async fn a_failing_orchestrator_is_named_above_the_overview() {
    let history = history(
        Range::Day,
        vec![bucket(
            hour(),
            ("blog.example.com", "blog", "web"),
            &[(200, 1, 4)],
        )],
    )
    .await;
    let overview = Overview::new(
        Readings {
            stacks: Err("Cannot show the stacks: the store is locked.".into()),
            history: Ok(history),
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

#[tokio::test]
async fn removed_stacks_keep_their_upstream_errors_and_deployments() {
    let history = history(
        Range::Day,
        vec![bucket(
            hour(),
            ("gone.example.com", "gone", "web"),
            &[(502, 1, 3), (200, 1, 2)],
        )],
    )
    .await;
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
            history: Ok(history),
            deployments: Ok(deployments),
            ..Readings::default()
        },
        SystemTime::now(),
    );
    let html = compact(&overview.render().unwrap());
    assert!(
        html.contains(r#"<span class="mono">gone (removed)</span> <span title="Responses over the last 24h: none 0 · 1xx 0 · 2xx 1 · 3xx 0 · 4xx 0 · 5xx 1">2 requests"#),
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
    assert!(matches!(
        OverviewEvents::changed(&mut events).await,
        Change::All
    ));
    assert!(events.try_recv().is_err(), "the burst was taken whole");
    drop(sender);
    assert!(matches!(
        OverviewEvents::changed(&mut events).await,
        Change::Closed
    ));
}

/// Event `id` of the event log, recorded now.
fn logged(id: i64, message: &str) -> LoggedEvent {
    LoggedEvent {
        id: EventId::from(id),
        at: Timestamp::now(),
        kind: EventKind::StackApplied,
        severity: Severity::Info,
        stack: Some("blog".into()),
        subject: Some("/stacks/blog".into()),
        message: message.into(),
        meta: None,
    }
}

#[tokio::test]
async fn a_burst_of_recorded_events_changes_only_the_events() {
    let (sender, mut events) = broadcast::channel(16);
    for id in 1..=2 {
        sender
            .send(UiEvent::Event(logged(id, "applied blog")))
            .unwrap();
    }
    match OverviewEvents::changed(&mut events).await {
        Change::Events(new) => assert_eq!(
            new.iter().map(|e| e.id).collect::<Vec<_>>(),
            [EventId::from(1), EventId::from(2)]
        ),
        other => panic!("{other:?}"),
    }
    // With anything else in the burst, the whole overview is read again.
    sender
        .send(UiEvent::Event(logged(3, "applied blog")))
        .unwrap();
    sender
        .send(UiEvent::Deployment {
            stack: "blog".into(),
            id: DeploymentId::from(1),
            state: DeploymentState::Pulling,
        })
        .unwrap();
    assert!(matches!(
        OverviewEvents::changed(&mut events).await,
        Change::All
    ));
    // A restart is the only report of the new restart count, so it reads everything too.
    sender
        .send(UiEvent::Event(LoggedEvent {
            kind: EventKind::ContainerRestarted,
            ..logged(4, "restarted blog/web")
        }))
        .unwrap();
    assert!(matches!(
        OverviewEvents::changed(&mut events).await,
        Change::All
    ));
}

#[tokio::test]
async fn a_lagged_receiver_reads_the_events_again() {
    let (sender, mut events) = broadcast::channel(1);
    for id in 1..=3 {
        sender
            .send(UiEvent::Event(logged(id, "applied blog")))
            .unwrap();
    }
    assert!(matches!(
        OverviewEvents::changed(&mut events).await,
        Change::All
    ));
}

#[tokio::test(start_paused = true)]
async fn a_quiet_stream_renders_again_after_the_refresh_interval() {
    let (_sender, mut events) = broadcast::channel::<UiEvent>(16);
    let started = tokio::time::Instant::now();
    assert!(matches!(
        OverviewEvents::changed(&mut events).await,
        Change::All
    ));
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
            r#"<div id="overview" class="dashboard" hx-sse:connect="/events?range=24h" hx-swap="innerHTML" hx-sse:close="end">"#
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
async fn a_recorded_event_sends_only_the_recent_events_card_with_it_on_top() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    *app.stacks.stacks.lock().unwrap() = vec![blog()];
    app.stacks
        .log
        .lock()
        .unwrap()
        .push(logged(1, "applied blog revision 1"));

    let html = compact(&body(app.get("/", Some(&browser.cookie)).await).await);
    assert!(
        html.contains(
            r#"<section class="card" id="recent-events" aria-labelledby="recent-events-title">"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/stacks/blog">applied blog revision 1</a>"#),
        "{html}"
    );

    let response = app.get("/events", Some(&browser.cookie)).await;
    let mut stream = response.into_body();
    let mut text = String::new();
    read_until(&mut stream, &mut text, "\n\n").await;
    assert!(text.contains("applied blog revision 1"), "{text}");

    // The event is recorded, then announced.
    let event = logged(2, "applied blog revision 2");
    app.stacks.log.lock().unwrap().push(event.clone());
    app.stacks.events.send(UiEvent::Event(event)).unwrap();
    text.clear();
    read_until(&mut stream, &mut text, "\n\n").await;
    assert!(
        text.starts_with(r##"data: <hx-partial hx-target="#recent-events" hx-swap="outerHTML">"##),
        "{text}"
    );
    assert!(!text.contains("Key figures"), "only the card: {text}");
    let newest = text.find("applied blog revision 2").expect(&text);
    assert!(
        newest < text.find("applied blog revision 1").expect(&text),
        "{text}"
    );
}

#[test]
fn the_overview_shows_the_recent_events_or_why_not() {
    let html = Overview::new(Readings::default(), SystemTime::now())
        .render()
        .unwrap();
    assert!(html.contains("No events yet."), "{html}");

    let html = Overview::new(
        Readings {
            events: Err("Cannot show the recent events: disk I/O error.".into()),
            ..Readings::default()
        },
        SystemTime::now(),
    )
    .render()
    .unwrap();
    assert!(
        html.contains(
            r#"<p class="flash flash-error">Cannot show the recent events: disk I/O error.</p>"#
        ),
        "{html}"
    );
    assert!(
        html.contains("The events cannot be read right now."),
        "{html}"
    );
}

#[tokio::test]
async fn the_stream_needs_a_session() {
    let app = TestApp::new().await;
    let response = app.get("/events", None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn every_range_renders_with_its_link_marked_and_the_stream_on_it() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    *app.stacks.stacks.lock().unwrap() = vec![blog()];
    for (query, range) in [
        ("", "24h"),
        ("?range=1h", "1h"),
        ("?range=6h", "6h"),
        ("?range=24h", "24h"),
        ("?range=7d", "7d"),
        ("?range=30d", "30d"),
        // An unknown range falls back to the default.
        ("?range=1y", "24h"),
    ] {
        let html = compact(&body(app.get(&format!("/{query}"), Some(&browser.cookie)).await).await);
        let picker = &html[html.find(r#"<div class="range-picker""#).expect(&html)..];
        let picker = &picker[..picker.find("</div>").unwrap()];
        for other in ["1h", "6h", "24h", "7d", "30d"] {
            let link = if other == range {
                format!(r#"<a href="/?range={other}" aria-current="page">{other}</a>"#)
            } else {
                format!(r#"<a href="/?range={other}">{other}</a>"#)
            };
            assert!(picker.contains(&link), "{query}: {picker}");
        }
        assert_eq!(picker.matches("aria-current").count(), 1, "{picker}");
        assert!(
            html.contains(&format!(r#"hx-sse:connect="/events?range={range}""#)),
            "{query}: {html}"
        );
        assert!(
            html.contains(&format!(
                r#"Responses by status <span class="card-count">· {range} · all stacks</span>"#
            )) && html.contains(&format!(
                r#"Response time <span class="card-count">· {range}</span>"#
            )),
            "{query}: {html}"
        );
        assert!(
            html.contains(&format!(r#"<dt class="kpi-label">requests · {range}</dt>"#)),
            "{query}: {html}"
        );
        assert_eq!(
            html.matches(r#"class="chart-plot""#).count(),
            2,
            "{query}: {html}"
        );
        assert!(!html.contains("style="), "{html}");
    }

    // The stream sends the overview on the range of its URL.
    let response = app.get("/events?range=7d", Some(&browser.cookie)).await;
    let mut stream = response.into_body();
    let mut text = String::new();
    read_until(&mut stream, &mut text, "\n\n").await;
    assert!(text.contains("requests · 7d"), "{text}");
    assert!(
        text.contains("Response time <span class=\"card-count\">· 7d"),
        "{text}"
    );
}

#[tokio::test]
async fn deltas_compare_with_the_period_before_and_are_left_out_without_one() {
    let blog_web = ("blog.example.com", "blog", "web");
    // Only the range has data: no delta on any tile.
    let only_now = overview_of(
        vec![blog()],
        history(
            Range::Hour,
            vec![bucket(hour(), blog_web, &[(200, 40, 20)])],
        )
        .await,
    );
    let tiles: Vec<String> = only_now.kpis.iter().map(ToString::to_string).collect();
    assert!(
        tiles[0]
            .contains(r#"<dd class="kpi-value">40</dd><dd class="kpi-sub">no previous data</dd>"#),
        "{tiles:?}"
    );
    for tile in &tiles[2..4] {
        assert!(
            tile.contains(r#"<dd class="kpi-sub">no previous data</dd>"#),
            "{tiles:?}"
        );
    }
    assert!(!tiles.concat().contains("vs prev"), "{tiles:?}");

    // The hour before had more, slower requests: everything went down.
    let both = overview_of(
        vec![blog()],
        history(
            Range::Hour,
            vec![
                bucket(hour(), blog_web, &[(200, 40, 20)]),
                bucket(hour() - MINUTE * 40, blog_web, &[(200, 50, 80)]),
            ],
        )
        .await,
    );
    let tiles: Vec<String> = both.kpis.iter().map(ToString::to_string).collect();
    assert!(
        tiles[0].contains(r#"<dd class="kpi-sub">▼ 20.0 % vs prev</dd>"#),
        "{tiles:?}"
    );
    assert!(
        tiles[2].contains("▼ ") && tiles[2].contains(" ms vs prev"),
        "{tiles:?}"
    );
    assert!(
        tiles[3].contains("▼ ") && tiles[3].contains(" ms vs prev"),
        "{tiles:?}"
    );
    // A change has no tone of its own.
    assert!(
        tiles[0].contains("kpi-neutral") && tiles[2].contains("kpi-neutral"),
        "{tiles:?}"
    );

    // The same figures: unchanged.
    let same = overview_of(
        vec![blog()],
        history(
            Range::Hour,
            vec![
                bucket(hour(), blog_web, &[(200, 40, 20)]),
                bucket(hour() - MINUTE * 40, blog_web, &[(200, 40, 20)]),
            ],
        )
        .await,
    );
    let tiles: Vec<String> = same.kpis.iter().map(ToString::to_string).collect();
    assert!(
        tiles[0].contains(r#"<dd class="kpi-sub">unchanged vs prev</dd>"#)
            && tiles[2].contains(r#"<dd class="kpi-sub">unchanged vs prev</dd>"#),
        "{tiles:?}"
    );
}

#[tokio::test]
async fn the_admin_uis_own_requests_are_not_counted() {
    // Live: one request in flight to blog, two to the admin UI.
    let traffic = Traffic::default();
    let _blog = traffic.route("blog.example.com", &entry(routed("blog", "web"), "/"));
    let admin = entry(Owner::Reserved("admin UI"), "/");
    let _admin = [
        traffic.route("admin.example.com", &admin),
        traffic.route("admin.example.com", &admin),
    ];
    let mut done = traffic.route("blog.example.com", &entry(routed("blog", "web"), "/"));
    finish(&mut done, 200, 3);
    drop(done);
    // In the history, only the admin UI saw requests.
    let history = history(
        Range::Day,
        vec![bucket(
            hour(),
            ("admin.example.com", "", "admin UI"),
            &[(200, 30, 2), (500, 3, 2)],
        )],
    )
    .await;
    let overview = Overview::new(
        Readings {
            stacks: Ok(vec![blog()]),
            history: Ok(history),
            inflight: traffic.in_flight().hosted(),
            ..Readings::default()
        },
        SystemTime::now(),
    );
    let tiles: Vec<String> = overview.kpis.iter().map(ToString::to_string).collect();
    assert!(
        tiles[0].contains(
            r#"<dd class="kpi-value">0</dd><dd class="kpi-sub">no previous data · 1 in flight</dd>"#
        ),
        "{tiles:?}"
    );
    assert!(
        tiles[1].contains(r#"<dd class="kpi-value">-</dd><dd class="kpi-sub">no requests</dd>"#)
            && !tiles[1].contains("kpi-fail"),
        "{tiles:?}"
    );
    assert!(
        tiles[2].contains(r#"<dd class="kpi-value">-</dd><dd class="kpi-sub">no requests</dd>"#),
        "{tiles:?}"
    );
    let html = compact(&overview.render().unwrap());
    // The charts show nothing for the admin UI's requests: an empty plot.
    assert!(html.contains("2xx 0"), "{html}");
    // The note counts them.
    assert!(
        html.contains(r#"<span class="mono">hoster (admin UI)</span> <span title="Responses over the last 24h: none 0 · 1xx 0 · 2xx 30 · 3xx 0 · 4xx 0 · 5xx 3">33 requests"#),
        "{html}"
    );
    // blog routes hosts but saw nothing: zero rate, `-` figures, a flat sparkline.
    let row = row(&html, "blog");
    assert!(
        row.contains(r#"<td class="num">0</td> <td class="num">-</td> <td class="num stat-neutral">-</td> <td class="num">-</td> <td class="num stat-neutral">-</td>"#),
        "{row}"
    );
    assert!(row.contains("0 requests</span><svg class=\"spark"), "{row}");
}

#[tokio::test]
async fn a_stack_without_routes_shows_dashes_and_an_unreadable_history_is_named() {
    const WORKER: &str = r#"version = 1
name = "jobs"

[services.worker]
image = "ghcr.io/acme/jobs"
"#;
    let mut jobs = blog();
    jobs.name = "jobs".into();
    jobs.revision.toml = WORKER.into();
    jobs.services.truncate(1);
    let overview = Overview::new(
        Readings {
            stacks: Ok(vec![jobs]),
            history: Err("Cannot show the traffic history: database is locked.".into()),
            ..Readings::default()
        },
        SystemTime::now(),
    );
    let html = compact(&overview.render().unwrap());
    assert!(
        html.contains(r#"<p class="flash flash-error">Cannot show the traffic history: database is locked.</p>"#),
        "{html}"
    );
    let row = row(&html, "jobs");
    assert_eq!(
        row.matches(r#"<td class="num muted" title="The stack routes nothing">-</td>"#)
            .count(),
        6,
        "{row}"
    );
    assert!(!row.contains("spark"), "{row}");
    // The charts still draw, empty, over the range.
    assert!(
        html.contains("Responses by status <span class=\"card-count\">· 24h"),
        "{html}"
    );
}
