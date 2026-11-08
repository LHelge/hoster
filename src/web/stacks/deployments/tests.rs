//! The deployment history, the live progress stream and "redeploy this one", through the whole
//! app against the fake orchestrator.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;

use crate::orchestrator::UiEvent;
use crate::store::{DeploymentId, DeploymentSource, DeploymentState, Provenance, Timestamp};
use crate::web::auth::tests::{ORIGIN, TestApp};
use crate::web::stacks::tests::{Call, FORM, body, deployment, digest, post};

/// Deployments 3 (live), 4 (a failed webhook deploy) and 5 (running) of `notes`.
fn with_history(app: &TestApp) {
    let mut failed = deployment(4, DeploymentState::Failed);
    failed.error = Some("health check of <api> timed out".into());
    failed.source = DeploymentSource::Webhook;
    failed.provenance = Some(Provenance {
        repository: "acme/notes".into(),
        sha: "4f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39".into(),
        git_ref: "refs/tags/v1.4.2".into(),
        run_id: "11223300".into(),
        actor: "octocat".into(),
        tag: Some("v1.4.2".into()),
    });
    failed.created_at = Timestamp::now() - Duration::from_secs(95);
    let mut live = deployment(3, DeploymentState::Live);
    live.created_at = Timestamp::now() - Duration::from_secs(600);
    live.finished_at = Some(live.created_at + Duration::from_secs(12));
    let running = deployment(5, DeploymentState::Pulling);
    let mut deployments = app.stacks.deployments.lock().unwrap();
    for d in [live, failed, running] {
        deployments.push((d, Vec::new()));
    }
}

#[tokio::test]
async fn the_history_shows_each_deployment_newest_first() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    with_history(&app);

    let stack = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        stack.contains(r#"href="/stacks/notes/deployments""#),
        "the stack page links the history: {stack}"
    );

    let response = app
        .get("/stacks/notes/deployments", Some(&browser.cookie))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    let rows: Vec<&str> = html.split("<tr>").skip(2).collect();
    assert_eq!(rows.len(), 3, "{html}");
    assert!(
        html.contains(r#"<a href="/stacks/notes">notes</a>/deployments</nav>"#),
        "the stack's header, its crumb ending in the tab: {html}"
    );
    assert!(
        html.contains(
            r#"href="/stacks/notes/deployments" aria-selected="true" aria-current="page""#
        ),
        "{html}"
    );
    // A running one: its pill, and the state it is in after the duration.
    assert!(rows[0].contains(r##"<a href="/stacks/notes/deployments/5">#5</a>"##));
    assert!(
        rows[0].contains(r#"title="Running: pulling""#) && rows[0].contains("running</span>"),
        "{}",
        rows[0]
    );
    assert!(rows[0].contains(" · pulling</span>"), "{}", rows[0]);
    assert!(rows[1].contains(">#4</a>"), "{}", rows[1]);
    assert!(
        rows[1].contains(&format!(
            r#"<span class="pill pill-fail" title="Failed, and the stack was put back as it was">{}failed</span>"#,
            crate::web::Icon::Failed
        )),
        "{}",
        rows[1]
    );
    // The trigger, with the webhook's commit and repository.
    assert!(
        rows[1].contains(r#"<span class="mono">webhook</span>"#),
        "{}",
        rows[1]
    );
    assert!(
        rows[1].contains(r#"<code title="4f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39">4f2a9c1</code> <span class="deploy-repo">acme/notes</span>"#),
        "{}",
        rows[1]
    );
    assert!(
        rows[1].contains(" · health check of &#60;api&#62; timed out</span>"),
        "the failure reason, escaped: {}",
        rows[1]
    );
    assert!(rows[2].contains(">#3</a>") && rows[2].contains("live</span>"));
    assert!(
        rows[2].contains(r#"<span class="mono nowrap">12.0 s</span>"#),
        "{}",
        rows[2]
    );
    assert!(rows[2].contains(">10 min ago</time>"), "{}", rows[2]);
    let short = format!(r#"<code title="{}">sha256:010101010101</code>"#, digest(1));
    assert!(rows[2].contains(&short), "{}", rows[2]);
    // Only the deployment that went live can be redeployed; it is the latest live one, so the
    // button says redeploy.
    assert!(
        rows[2].contains(
            r##"hx-post="/stacks/notes/deployments/3/redeploy" hx-target="#redeploy-result" hx-confirm="Redeploy deployment 3?"##
        ),
        "{}",
        rows[2]
    );
    assert!(rows[2].contains("redeploy<span"), "{}", rows[2]);
    assert!(!rows[0].contains("/redeploy") && !rows[1].contains("/redeploy"));

    let missing = app
        .get("/stacks/other/deployments", Some(&browser.cookie))
        .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn redeploying_a_live_deployment_rolls_back_and_follows_the_new_one() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    with_history(&app);

    let page = body(
        app.get("/stacks/notes/deployments/3", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(page.contains("Redeploy this one"), "{page}");
    assert!(page.contains("took 12.0 s"), "{page}");
    assert!(
        page.contains(r#"<a href="/stacks/notes">notes</a>/<a href="/stacks/notes/deployments">deployments</a>/3</nav>"#),
        "the stack's header, its crumb ending in the deployment: {page}"
    );
    assert!(
        page.contains(r#"href="/stacks/notes/deployments" aria-selected="true">"#),
        "the deployments tab is selected, but is not the page: {page}"
    );
    assert!(
        !page.contains("hx-sse:connect"),
        "a finished deployment is not followed: {page}"
    );

    let response = app
        .send(post(
            "/stacks/notes/deployments/3/redeploy",
            &browser,
            FORM,
            String::new(),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["hx-redirect"],
        "/stacks/notes/deployments/20"
    );
    let html = body(response).await;
    assert!(html.contains("deployment 20"), "{html}");
    assert_eq!(
        app.stacks.calls(),
        [Call::Rollback("notes".into(), DeploymentId::from(3))]
    );

    // Already running: a Noop, said so, and nothing to follow.
    *app.stacks.deploy_state.lock().unwrap() = DeploymentState::Noop;
    let noop = app
        .send(post(
            "/stacks/notes/deployments/3/redeploy",
            &browser,
            FORM,
            String::new(),
        ))
        .await;
    assert!(noop.headers().get("hx-redirect").is_none());
    assert!(body(noop).await.contains("Noop"));

    let refused = app
        .send(post(
            "/stacks/notes/deployments/4/redeploy",
            &browser,
            FORM,
            String::new(),
        ))
        .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body(refused).await.contains("never went live"));

    let missing = app
        .send(post(
            "/stacks/notes/deployments/x/redeploy",
            &browser,
            FORM,
            String::new(),
        ))
        .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn redeploy_needs_the_csrf_token() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    with_history(&app);
    let request = Request::post("/stacks/notes/deployments/3/redeploy")
        .header(header::COOKIE, &browser.cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, FORM)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(request).await.status(), StatusCode::FORBIDDEN);
    assert!(app.stacks.calls().is_empty());
}

/// Reads the stream until its text so far contains `needle`.
async fn read_until(body: &mut Body, text: &mut String, needle: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !text.contains(needle) {
            let frame = body
                .frame()
                .await
                .expect("the stream is still open")
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
async fn the_progress_stream_sends_each_change_until_the_deployment_ends() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    with_history(&app);

    let response = app
        .get("/stacks/notes/deployments/5/events", Some(&browser.cookie))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut stream = response.into_body();
    let mut text = String::new();
    // The current status at once.
    read_until(&mut stream, &mut text, "<code>Pulling</code>").await;
    assert!(
        text.starts_with("data: <div id=\"deployment-status\""),
        "{text}"
    );
    assert!(text.contains("Cancel deployment"), "{text}");

    // Another deployment's change is not this one's.
    let set = |state| {
        app.stacks.deployments.lock().unwrap()[2].0 = deployment(5, state);
    };
    set(DeploymentState::Starting);
    let event = |id: i64, state| UiEvent::Deployment {
        stack: "notes".into(),
        id: DeploymentId::from(id),
        state,
    };
    app.stacks
        .events
        .send(event(4, DeploymentState::Failed))
        .unwrap();
    app.stacks
        .events
        .send(event(5, DeploymentState::Starting))
        .unwrap();
    text.clear();
    read_until(&mut stream, &mut text, "<code>Starting</code>").await;
    assert!(!text.contains("event: end"), "{text}");

    set(DeploymentState::Live);
    app.stacks
        .events
        .send(event(5, DeploymentState::Live))
        .unwrap();
    let rest = body(axum::response::Response::new(stream)).await;
    assert!(rest.contains("pill pill-ok"), "{rest}");
    assert!(rest.contains("live</span>"), "{rest}");
    assert!(rest.contains("Redeploy this one"), "{rest}");
    assert!(!rest.contains("Cancel deployment"), "{rest}");
    assert!(rest.trim_end().ends_with("event: end"), "{rest:?}");
}

#[tokio::test]
async fn a_finished_deployments_stream_ends_at_once() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    with_history(&app);
    let response = app
        .get("/stacks/notes/deployments/3/events", Some(&browser.cookie))
        .await;
    let text = body(response).await;
    assert!(
        text.contains(&format!(
            r#"<span class="pill pill-ok" title="Went live">{}live</span>"#,
            crate::web::Icon::Ok
        )),
        "{text}"
    );
    assert!(text.trim_end().ends_with("event: end"), "{text:?}");

    for path in [
        "/stacks/other/deployments/3/events",
        "/stacks/notes/deployments/99/events",
        "/stacks/notes/deployments/x/events",
    ] {
        let response = app.get(path, Some(&browser.cookie)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let signed_out = app.get("/stacks/notes/deployments/3/events", None).await;
    assert_eq!(signed_out.status(), StatusCode::SEE_OTHER);
}

#[test]
fn the_status_says_what_became_of_a_deployment() {
    use super::{Phase, Status};
    let status = |state, error: Option<&str>| {
        let mut d = deployment(1, state);
        d.error = error.map(str::to_owned);
        Status::from(&d)
    };
    assert_eq!(status(DeploymentState::Live, None).to_string(), "live");
    assert_eq!(
        status(DeploymentState::LiveWithWarnings, None).to_string(),
        "live · warnings"
    );
    assert_eq!(
        status(DeploymentState::Failed, Some("pull failed")).to_string(),
        "failed"
    );
    assert_eq!(
        status(
            DeploymentState::Failed,
            Some(crate::orchestrator::CANCELLED)
        ),
        Status::Cancelled
    );
    assert_eq!(status(DeploymentState::Noop, None).to_string(), "skipped");
    assert_eq!(
        status(DeploymentState::Superseded, None).to_string(),
        "superseded"
    );
    assert_eq!(status(DeploymentState::Queued, None).to_string(), "queued");
    assert_eq!(
        status(DeploymentState::Rejected, None).to_string(),
        "rejected"
    );
    let running = status(DeploymentState::HealthChecking, None);
    assert_eq!(running.to_string(), "running");
    assert_eq!(
        running.phase().map(|p| p.to_string()).as_deref(),
        Some("health checking")
    );
    assert_eq!(
        Phase(DeploymentState::CleaningUp).to_string(),
        "cleaning up"
    );
}

#[tokio::test]
async fn an_older_live_deployment_offers_a_rollback() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    with_history(&app);
    let mut older = deployment(2, DeploymentState::Live);
    older.source = DeploymentSource::Manual;
    let mut cancelled = deployment(1, DeploymentState::Failed);
    cancelled.error = Some(crate::orchestrator::CANCELLED.into());
    app.stacks
        .deployments
        .lock()
        .unwrap()
        .extend([(older, Vec::new()), (cancelled, Vec::new())]);

    let html = body(
        app.get("/stacks/notes/deployments", Some(&browser.cookie))
            .await,
    )
    .await;
    let rows: Vec<&str> = html.split("<tr>").skip(2).collect();
    assert_eq!(rows.len(), 5, "{html}");
    assert!(rows[2].contains("redeploy<span"), "{}", rows[2]);
    assert!(rows[3].contains(">#2</a>"), "{}", rows[3]);
    assert!(
        rows[3].contains(r#"hx-confirm="Roll back to deployment 2?"#),
        "{}",
        rows[3]
    );
    assert!(rows[3].contains("roll back<span"), "{}", rows[3]);
    assert!(rows[4].contains("cancelled</span>"), "{}", rows[4]);
    assert!(
        !rows[4].contains(" · cancelled"),
        "the pill says it, not the reason again: {}",
        rows[4]
    );

    let page = body(
        app.get("/stacks/notes/deployments/2", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(page.contains("Roll back to this one"), "{page}");
    assert!(page.contains("Roll back to deployment 2?"), "{page}");
}

#[tokio::test]
async fn a_webhook_deployments_page_lists_its_provenance() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    with_history(&app);
    let page = body(
        app.get("/stacks/notes/deployments/4", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(
        page.contains(
            r#"<p class="card-body card-error">health check of &#60;api&#62; timed out</p>"#
        ),
        "{page}"
    );
    assert!(
        page.contains(r#"<dt>commit</dt><dd><code class="wrap">4f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39</code></dd>"#),
        "{page}"
    );
    assert!(
        page.contains(r#"<dt>tag</dt><dd><code class="wrap">v1.4.2</code></dd>"#),
        "{page}"
    );
    assert!(
        page.contains(&format!(
            r#"<dt>api</dt><dd><code class="wrap">{}</code></dd>"#,
            digest(1)
        )),
        "the target digests in full: {page}"
    );
    assert!(
        !page.contains("deployments/4/redeploy"),
        "a failed one cannot be redeployed: {page}"
    );
}

#[tokio::test]
async fn a_removed_stacks_deployment_still_shows_without_the_stack_header() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let mut gone = deployment(9, DeploymentState::Live);
    gone.stack = "gone".into();
    app.stacks
        .deployments
        .lock()
        .unwrap()
        .push((gone, Vec::new()));
    let response = app
        .get("/stacks/gone/deployments/9", Some(&browser.cookie))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page = body(response).await;
    assert!(page.contains("<h1>Deployment 9 of gone</h1>"), "{page}");
    assert!(!page.contains(r#"class="tabs""#), "{page}");
}

#[tokio::test]
async fn a_stack_without_deployments_says_so() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = body(
        app.get("/stacks/notes/deployments", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(html.contains("No deployments yet."), "{html}");
    assert!(!html.contains("<table"), "{html}");
}
