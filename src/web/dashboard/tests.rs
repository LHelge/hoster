//! The dashboard's overview: its warnings and table rendered on their own, and the page and its
//! live stream through the whole app against the fake orchestrator.

use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::StatusCode;
use http_body_util::BodyExt as _;

use super::*;
use crate::health::HealthStats;
use crate::orchestrator::RestartStats;
use crate::store::{
    ContentHash, DeploymentId, DeploymentSource, DigestSet, RevisionId, StackRevision, Timestamp,
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

#[test]
fn a_healthy_stack_raises_no_warnings() {
    let now = SystemTime::now();
    let mut stack = blog();
    stack.services[1] = service("worker", HealthStatus::Healthy, 1, false);
    let certs = [
        cert("blog.example.com", 60 * DAY, now),
        cert("www.blog.example.com", 30 * DAY, now),
    ];
    let overview = Overview::new(&[stack], &certs, now);
    assert!(overview.warnings.is_empty(), "{:?}", overview.warnings);

    let html = compact(&overview.render().unwrap());
    assert!(html.contains("No warnings."), "{html}");
    let short = format!(r#"<code title="{}">sha256:010101010101</code>"#, digest(1));
    assert!(html.contains(&short), "{html}");
    assert!(
        html.contains(r#"<span class="badge badge-ok" title="GET /healthz of &#60;web&#62;: 503">healthy</span>"#),
        "{html}"
    );
    assert!(html.contains("blog.example.com: <span title="), "{html}");
    assert!(html.contains(">60 d left</span>"), "{html}");
    assert!(html.contains(">30 d left</span>"), "{html}");
    assert!(
        html.contains(r#"<a href="/stacks/blog/deployments/7">#7</a> Live"#),
        "{html}"
    );
    assert!(html.contains(r#"href="/stacks/blog/deployments">history</a>"#));
    assert!(!html.contains("badge-error"), "{html}");
}

#[test]
fn every_warning_kind_is_raised_and_badged() {
    let now = SystemTime::now();
    let mut blog = blog();
    blog.services[0] = service("web", HealthStatus::Unhealthy, 0, false);
    let mut failed = deployment(8, DeploymentState::Failed);
    failed.error = Some("health check of <web> timed out".into());
    blog.last = Some(failed);
    let certs = [
        cert("admin.example.com", 2 * DAY, now),
        cert("blog.example.com", 13 * DAY, now),
        cert("www.blog.example.com", 15 * DAY, now),
    ];
    let overview = Overview::new(&[blog], &certs, now);
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
        html.contains(
            r#"<span class="badge badge-error">failing health check</span> <a href="/stacks/blog">web (blog)</a>"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="badge badge-error">crash looping</span> <a href="/stacks/blog">worker (blog)</a>"#),
        "{html}"
    );
    assert!(
        html.contains(
            r#"<span class="badge badge-error">deploy failed</span> <a href="/stacks/blog/deployments/8">blog</a>: deployment 8 Failed: health check of &#60;web&#62; timed out"#
        ),
        "the detail is escaped: {html}"
    );
    assert!(
        html.contains(r#"<span class="badge badge-error">certificate expiring</span> <a href="/certificates">blog.example.com</a>"#),
        "{html}"
    );
    // In the table: the failed deployment, the expiring certificate, the crash loop.
    assert!(
        html.contains(r#"#8</a> <span class="badge badge-error">Failed</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="badge badge-error" title="#),
        "{html}"
    );
    assert!(html.contains(">13 d left</span>"), "{html}");
    assert!(
        html.contains(
            r#"<td><span class="badge badge-error">crash looping</span></td> <td>6</td>"#
        ),
        "{html}"
    );
}

#[test]
fn a_crash_loop_reported_by_the_health_monitor_counts_too() {
    let now = SystemTime::now();
    let mut blog = blog();
    blog.services[1] = service("worker", HealthStatus::CrashLooping, 0, false);
    let overview = Overview::new(&[blog], &[], now);
    assert_eq!(
        kinds(&overview),
        [(WarningKind::CrashLooping, "worker (blog)")]
    );
}

#[test]
fn services_without_a_generation_show_their_state_and_no_health() {
    let now = SystemTime::now();
    let mut blog = blog();
    blog.services[0].state = ServiceState::AwaitingFirstDigest;
    blog.services[0].health = HealthStats::default();
    // A crash-loop flag left from an old generation means nothing without one running.
    blog.services[1].state = ServiceState::NotDeployed;
    blog.active = Some((DeploymentId::from(9), DeploymentState::Pulling));
    blog.queued = Some(DeploymentId::from(10));
    let overview = Overview::new(&[blog], &[], now);
    assert!(overview.warnings.is_empty(), "{:?}", overview.warnings);
    let html = compact(&overview.render().unwrap());
    assert!(
        html.contains(r#"<td><span class="muted">awaiting its first digest</span></td> <td><span class="muted">-</span></td>"#),
        "{html}"
    );
    assert!(html.contains("blog.example.com: <span class=\"muted\">none yet</span>"));
    assert!(
        html.contains("<span class=\"muted\">not routed</span>"),
        "{html}"
    );
    assert!(
        html.contains(r#"#9</a> Pulling <span class="muted">(running)</span>"#),
        "{html}"
    );
    assert!(html.contains("#10 queued"), "{html}");
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
    let certs = [cert("blog.example.com", 2 * DAY, now)];
    let overview = Overview::new(&[blog], &certs, now);
    assert_eq!(
        kinds(&overview),
        [
            (WarningKind::DeployFailed, "blog"),
            (WarningKind::CertExpiring, "blog.example.com"),
        ]
    );
    let html = compact(&overview.render().unwrap());
    assert!(
        html.contains(r#"<th scope="rowgroup" rowspan="1"><a href="/stacks/blog">blog</a><br><span class="badge badge-error">revision 1: deploy failed: boom</span></th>"#),
        "{html}"
    );
    assert!(
        html.contains("no services: the active revision cannot be read"),
        "{html}"
    );
    assert!(
        html.contains(r#"<td rowspan="1"><a href="/stacks/blog/deployments/8">#8</a>"#),
        "{html}"
    );
}

#[test]
fn every_stack_heads_as_many_rows_as_it_has_services() {
    let html = compact(
        &Overview::new(&[blog()], &[], SystemTime::now())
            .render()
            .unwrap(),
    );
    assert!(
        html.contains(
            r#"<th scope="rowgroup" rowspan="2"><a href="/stacks/blog">blog</a></th> <td>web</td>"#
        ),
        "{html}"
    );
    assert_eq!(html.matches(r#"<td rowspan="2">"#).count(), 1, "{html}");
}

#[test]
fn an_empty_overview_says_so() {
    let html = Overview::new(&[], &[], SystemTime::now()).render().unwrap();
    assert!(html.contains("No warnings."));
    assert!(html.contains("No stacks yet."));
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
async fn the_dashboard_shows_the_stacks_in_a_live_region() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    *app.stacks.stacks.lock().unwrap() = vec![blog()];
    app.stats.certificates.lock().unwrap().push(CertExpiry {
        host: "blog.example.com".into(),
        not_after: SystemTime::now() + 3 * DAY + Duration::from_secs(60),
    });

    let html = body(app.get("/", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(
            r#"<div id="overview" hx-sse:connect="/events" hx-swap="innerHTML" hx-sse:close="end">"#
        ),
        "{html}"
    );
    let html = compact(&html);
    assert!(
        html.contains(
            r#"<a href="/certificates">blog.example.com</a>: 3 d left, used by web (blog)"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/stacks/blog">worker (blog)</a>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/stacks/blog">blog</a>"#),
        "{html}"
    );
    assert!(
        html.contains("<h3>Traffic</h3>"),
        "the statistics follow: {html}"
    );
}

#[test]
fn a_failing_orchestrator_is_named_above_the_overview() {
    let overview = Overview {
        problem: Some("Cannot show the stacks: the store is locked.".into()),
        ..Overview::default()
    };
    let html = overview.render().unwrap();
    assert!(
        html.contains(
            r#"<p class="flash flash-error">Cannot show the stacks: the store is locked.</p>"#
        ),
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
async fn the_stream_sends_the_overview_again_on_every_change() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    *app.stacks.stacks.lock().unwrap() = vec![blog()];

    let response = app.get("/events", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut stream = response.into_body();
    let mut text = String::new();
    read_until(&mut stream, &mut text, "</section>").await;
    assert!(
        text.starts_with("data: <section id=\"overview-status\">"),
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
    read_until(&mut stream, &mut text, "</section>").await;
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
