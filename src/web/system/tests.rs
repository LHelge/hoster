//! The System page through the whole app, with the updater's fake release source on a temporary
//! host (an installed layout and a migrated database).

use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Request, header};

use super::*;
use crate::store::Store;
use crate::update::fake::{FakeReleases, Host, TARGET, script, v};
use crate::update::{Release, Sha256Digest};
use crate::web::auth::tests::{Browser, ORIGIN, TestApp};
use crate::web::stacks::tests::{FORM, body, encoded, post};

/// The app with its System page on `host`, running `running`, releases from `source`. Returns
/// the shutdown request the page fires after an update.
async fn app_on(host: &Host, source: FakeReleases, running: &str) -> (TestApp, ShutdownRequest) {
    let mut app = TestApp::new().await;
    let shutdown = ShutdownRequest::default();
    app.app.system = System::new(host.updater(source, running), shutdown.clone());
    (app, shutdown)
}

fn schema() -> i64 {
    Store::expected_schema_version().into()
}

async fn check(app: &TestApp, browser: &Browser) -> (StatusCode, String) {
    let response = app.send(post("/system/check", browser, FORM, "")).await;
    (response.status(), body(response).await)
}

async fn update(app: &TestApp, browser: &Browser, version: &str) -> (StatusCode, String) {
    let response = app
        .send(post(
            "/system/update",
            browser,
            FORM,
            encoded(&[("version", version)]),
        ))
        .await;
    (response.status(), body(response).await)
}

#[tokio::test]
async fn the_page_shows_version_target_schema_and_installed_versions() {
    let host = Host::new(&[("0.0.9", schema()), ("0.1.0", schema())]).await;
    let (app, _) = app_on(&host, FakeReleases::default(), "0.1.0").await;
    let browser = app.sign_in().await;
    let response = app.get("/system", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains(&format!(
            r#"<a href="/system" aria-current="page">{}System</a>"#,
            crate::web::Icon::System
        )),
        "{html}"
    );
    assert!(
        html.contains("<dt>Running version</dt><dd><code>v0.1.0</code></dd>"),
        "{html}"
    );
    assert!(html.contains(&format!("<code>{TARGET}</code>")), "{html}");
    assert!(
        html.contains(&format!("<code>{}</code>", schema())),
        "{html}"
    );
    assert!(
        html.contains("<dt>Installed (previous)</dt><dd><code>v0.0.9</code></dd>"),
        "{html}"
    );
    assert!(html.contains(r#"hx-post="/system/check""#), "{html}");
    // Rollback is CLI-only; the page says how.
    assert!(html.contains("<code>hoster rollback</code>"), "{html}");
    assert!(!html.contains("/system/rollback"), "{html}");
}

#[tokio::test]
async fn checking_when_up_to_date() {
    let host = Host::new(&[("0.2.0", schema())]).await;
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, shutdown) = app_on(&host, source, "0.2.0").await;
    let browser = app.sign_in().await;
    let (status, html) = check(&app, &browser).await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("hoster v0.2.0 is up to date"), "{html}");
    assert!(!html.contains("/system/update"), "{html}");
    assert!(!shutdown.is_requested());
}

#[tokio::test]
async fn checking_shows_the_new_version_with_escaped_notes_and_the_update_button() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let mut source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    source.releases[0].notes = "## Features\n- <script>alert(1)</script> & more".into();
    let (app, shutdown) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    let (status, html) = check(&app, &browser).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("v0.2.0 is available; this is v0.1.0"),
        "{html}"
    );
    assert!(
        html.contains(r#"<pre class="release-notes">## Features"#),
        "{html}"
    );
    assert!(
        html.contains("&#60;script&#62;alert(1)&#60;/script&#62; &#38; more"),
        "{html}"
    );
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains(r#"hx-post="/system/update""#), "{html}");
    assert!(html.contains(r#"name="version" value="v0.2.0""#), "{html}");
    assert!(html.contains("Update to v0.2.0"), "{html}");
    assert!(!html.contains("disabled"), "{html}");
    // Checking applies nothing.
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert!(!shutdown.is_requested());
}

#[tokio::test]
async fn updating_switches_and_requests_a_graceful_shutdown() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, shutdown) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Switched from v0.1.0 to v0.2.0"), "{html}");
    assert!(html.contains("Restarting"), "{html}");
    assert!(
        html.contains(r#"hx-get="/system/version?from=v0.1.0" hx-trigger="every 2s""#),
        "{html}"
    );
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert!(
        shutdown.is_requested(),
        "the supervisor is asked to restart"
    );
}

#[tokio::test]
async fn once_updated_hoster_refuses_another_update_until_it_restarted() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, shutdown) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(shutdown.is_requested());

    // The old binary still answers until the supervisor has shut it down.
    let (_, html) = check(&app, &browser).await;
    assert!(html.contains("disabled"), "{html}");
    assert!(html.contains("hoster is restarting"), "{html}");
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    assert!(html.contains("hoster is restarting"), "{html}");
    assert_eq!(host.current(), Some(v("0.2.0")));
}

#[tokio::test]
async fn a_second_update_is_refused_while_one_runs() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, shutdown) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    // Another request is in the middle of an update.
    let running = app.app.system.0.updating.lock().await;
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    assert!(html.contains("Another update is running"), "{html}");
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert!(!shutdown.is_requested());

    drop(running);
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert_eq!(host.current(), Some(v("0.2.0")));
}

#[tokio::test]
async fn the_restart_poll_reloads_once_another_version_answers() {
    let host = Host::new(&[("0.2.0", schema())]).await;
    let (app, _) = app_on(&host, FakeReleases::default(), "0.2.0").await;
    let browser = app.sign_in().await;
    let still_old = app
        .get("/system/version?from=v0.2.0", Some(&browser.cookie))
        .await;
    assert_eq!(still_old.status(), StatusCode::NO_CONTENT);
    assert!(still_old.headers().get("hx-refresh").is_none());

    let restarted = app
        .get("/system/version?from=v0.1.0", Some(&browser.cookie))
        .await;
    assert_eq!(restarted.status(), StatusCode::OK);
    assert_eq!(restarted.headers()["hx-refresh"], "true");
}

#[tokio::test]
async fn updating_is_refused_while_deploying() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    host.deployments.0.store(1, Ordering::SeqCst);
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, shutdown) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;

    // The button is disabled with the reason.
    let (_, html) = check(&app, &browser).await;
    assert!(html.contains("disabled"), "{html}");
    assert!(html.contains("1 deployment(s) are in progress"), "{html}");
    assert!(!html.contains(r#"hx-post="/system/update""#), "{html}");

    // A POST anyway is refused server-side.
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(html.contains("flash-error"), "{html}");
    assert!(html.contains("in progress"), "{html}");
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert!(!shutdown.is_requested());
}

#[tokio::test]
async fn a_checksum_failure_is_shown_and_nothing_is_applied() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let name = Release::binary_name(TARGET);
    let sums = format!(
        "{}  {name}\n",
        Sha256Digest::from(b"something else".as_slice())
    );
    let source = FakeReleases::default().publish(
        "0.2.0",
        &[
            (&name, script("0.2.0", schema())),
            (Release::CHECKSUMS, sums.into_bytes()),
        ],
    );
    let (app, shutdown) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(html.contains("flash-error"), "{html}");
    assert!(html.contains("does not match SHA256SUMS"), "{html}");
    assert!(html.contains("nothing was installed"), "{html}");
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert_eq!(host.installation.versions().unwrap(), [v("0.1.0")]);
    assert!(!shutdown.is_requested());
}

#[tokio::test]
async fn errors_from_github_and_bad_versions_are_shown_inline() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let (app, shutdown) = app_on(&host, FakeReleases::default(), "0.1.0").await;
    let browser = app.sign_in().await;
    let (status, html) = check(&app, &browser).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        html.contains("No stable hoster release is published yet"),
        "{html}"
    );

    let (status, html) = update(&app, &browser, "v0.9.0").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("There is no hoster release v0.9.0"), "{html}");

    let (status, _) = update(&app, &browser, "not a version").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!shutdown.is_requested());
}

#[tokio::test]
async fn updating_needs_the_versioned_layout() {
    let host = Host::new(&[]).await;
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, shutdown) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    let (_, html) = check(&app, &browser).await;
    assert!(html.contains("not installed with install.sh"), "{html}");
    let (status, _) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(host.current(), None);
    assert!(!shutdown.is_requested());
}

#[tokio::test]
async fn system_posts_need_the_csrf_token() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, shutdown) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    for uri in ["/system/check", "/system/update"] {
        let request = Request::post(uri)
            .header(header::COOKIE, &browser.cookie)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, FORM)
            .body(Body::from(encoded(&[("version", "v0.2.0")])))
            .unwrap();
        let response = app.send(request).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri}");
    }
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert!(!shutdown.is_requested());
}
