//! The deployment history, the live progress stream and "redeploy this one", through the whole
//! app against the fake orchestrator.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt as _;

use crate::orchestrator::UiEvent;
use crate::store::{DeploymentId, DeploymentSource, DeploymentState, Timestamp};
use crate::web::auth::tests::{ORIGIN, TestApp};
use crate::web::stacks::tests::{Call, FORM, body, deployment, digest, post};

/// Deployments 3 (live), 4 (failed) and 5 (running) of `notes`.
fn with_history(app: &TestApp) {
    let mut failed = deployment(4, DeploymentState::Failed);
    failed.error = Some("health check of <api> timed out".into());
    failed.source = DeploymentSource::Webhook;
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
    assert!(rows[0].contains(">5</a>") && rows[0].contains("Pulling"));
    assert!(rows[0].contains("(running)"), "{}", rows[0]);
    assert!(rows[1].contains(">4</a>"), "{}", rows[1]);
    assert!(
        rows[1].contains(&format!(
            r#"<span class="pill pill-fail">{}Failed</span>"#,
            crate::web::Icon::Failed
        )),
        "{}",
        rows[1]
    );
    assert!(rows[1].contains("<td>Webhook</td>"), "{}", rows[1]);
    assert!(
        rows[1].contains("health check of &#60;api&#62; timed out"),
        "the failure reason, escaped: {}",
        rows[1]
    );
    assert!(rows[2].contains(">3</a>") && rows[2].contains("Live"));
    assert!(rows[2].contains("<td>12.0 s</td>"), "{}", rows[2]);
    let short = format!(r#"<code title="{}">sha256:010101010101</code>"#, digest(1));
    assert!(rows[2].contains(&short), "{}", rows[2]);
    // Only the deployment that went live can be redeployed.
    assert!(
        rows[2].contains(
            r##"hx-post="/stacks/notes/deployments/3/redeploy" hx-target="#redeploy-result""##
        ),
        "{}",
        rows[2]
    );
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
    assert!(page.contains("after 12.0 s"), "{page}");
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
    read_until(&mut stream, &mut text, "Pulling</span>").await;
    assert!(
        text.starts_with("data: <section id=\"deployment-status\">"),
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
    read_until(&mut stream, &mut text, "Starting</span>").await;
    assert!(!text.contains("event: end"), "{text}");

    set(DeploymentState::Live);
    app.stacks
        .events
        .send(event(5, DeploymentState::Live))
        .unwrap();
    let rest = body(axum::response::Response::new(stream)).await;
    assert!(rest.contains("pill-ok\">"), "{rest}");
    assert!(rest.contains("Live</span>"), "{rest}");
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
            r#"<span class="pill pill-ok">{}Live</span>"#,
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
