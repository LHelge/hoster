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
    app.app.system = System::new(
        host.updater(source, running),
        shutdown.clone(),
        Arc::new(Facts::sample()),
    );
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
        html.contains(r#"<span class="version-now">v0.1.0</span>"#),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            "<code>{TARGET}</code> · schema <code>{}</code> · up ",
            schema()
        )),
        "{html}"
    );
    assert!(
        html.contains("installed current: <code>v0.1.0</code>"),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="version-prev">v0.0.9</span>"#),
        "{html}"
    );
    // The check is the header's action; the result goes into the update card.
    assert!(
        html.contains(r##"hx-post="/system/check" hx-target="#update-check""##),
        "{html}"
    );
    assert!(html.contains(r#"id="update-check""#), "{html}");
    // Rollback is CLI-only; the page says how and links the manual recovery.
    assert!(
        html.contains("ssh … &amp;&amp; hoster rollback</code>"),
        "{html}"
    );
    assert!(
        html.contains(r#"href="https://docs.hoster.lhelge.se/wiki/install/#manual-recovery""#),
        "{html}"
    );
    assert!(!html.contains("/system/rollback"), "{html}");
    assert!(!html.contains("roll back</button>"), "{html}");
}

#[tokio::test]
async fn the_page_explains_why_there_is_nothing_to_roll_back_to() {
    // One version installed: nothing older to return to.
    let host = Host::new(&[("0.1.0", schema())]).await;
    let (app, _) = app_on(&host, FakeReleases::default(), "0.1.0").await;
    let browser = app.sign_in().await;
    let html = body(app.get("/system", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<span class="version-prev muted">none</span>"#),
        "{html}"
    );
    assert!(html.contains("nothing to roll back to"), "{html}");
    assert!(!html.contains("not installed with"), "{html}");

    // No versioned layout at all.
    let host = Host::new(&[]).await;
    let (app, _) = app_on(&host, FakeReleases::default(), "0.1.0").await;
    let browser = app.sign_in().await;
    let html = body(app.get("/system", Some(&browser.cookie)).await).await;
    assert!(
        html.contains("installed current: <span class=\"muted\">none</span>"),
        "{html}"
    );
    assert!(
        html.contains("not installed with <code>install.sh</code>"),
        "{html}"
    );
    assert!(!html.contains("nothing to roll back to"), "{html}");
}

#[tokio::test]
async fn an_unreadable_layout_is_unknown_not_missing() {
    let host = Host::new(&[("0.0.9", schema()), ("0.1.0", schema())]).await;
    // `current` points outside `versions/`: hoster cannot tell what is installed.
    let link = host.installation.root().join("current");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("/elsewhere/v0.1.0", &link).unwrap();
    let (app, _) = app_on(&host, FakeReleases::default(), "0.1.0").await;
    let browser = app.sign_in().await;
    let response = app.get("/system", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("Cannot read the installed versions: "),
        "{html}"
    );
    assert!(
        html.contains("installed current: <span class=\"muted\">unknown</span>"),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="version-prev muted">unknown</span>"#),
        "{html}"
    );
    assert!(!html.contains("not installed with"), "{html}");
    assert!(!html.contains("nothing to roll back to"), "{html}");
}

#[tokio::test]
async fn the_page_shows_the_host_facts_and_unknown_for_missing_ones() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let (mut app, _) = app_on(&host, FakeReleases::default(), "0.1.0").await;
    let browser = app.sign_in().await;
    let html = body(app.get("/system", Some(&browser.cookie)).await).await;
    for row in [
        "<dt>hostname</dt>\n    <dd>srv-01</dd>",
        "<dt>os</dt>\n    <dd>Debian GNU/Linux 13 (trixie)</dd>",
        "<dt>kernel</dt>\n    <dd>6.12.48-amd64</dd>",
        "<dt>arch</dt>\n    <dd>x86_64 · 8 CPU</dd>",
        "<dt>memory</dt>\n    <dd>32.0 GiB</dd>",
        "<dt>runtime</dt>\n    <dd>podman 5.4.2 · rootless</dd>",
        "<dt>cgroups</dt>\n    <dd>v2 · systemd</dd>",
        "<dd>203.0.113.42 · 2001:db8::42 <span class=\"host-note\">· the A/AAAA records of admin.example.com</span></dd>",
        "<dt>data dir</dt>\n    <dd>/home/hoster/.local/state/hoster</dd>",
        "<dt>uptime</dt>\n    <dd>41d 7h</dd>",
        "<dt>open files</dt>\n    <dd>214 of 16384 <span class=\"host-note\">· new clients refused from 4096</span></dd>",
        "<dt>refused</dt>\n    <dd>0 client connections since start</dd>",
    ] {
        assert!(html.contains(row), "{row}\n{html}");
    }

    // Nothing known still renders the page, every fact as unknown.
    app.app.system = System::new(
        host.updater(FakeReleases::default(), "0.1.0"),
        ShutdownRequest::default(),
        Arc::new(Facts::default()),
    );
    let response = app.get("/system", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert_eq!(
        html.matches(r#"<dd><span class="muted">unknown</span></dd>"#)
            .count(),
        12,
        "{html}"
    );
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
    assert!(html.contains("Update available"), "{html}");
    assert!(
        html.contains(r#"<span class="update-version">v0.2.0 <span class="update-meta">· this is v0.1.0</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"aria-label="Release notes for v0.2.0">## Features"#),
        "{html}"
    );
    assert!(
        html.contains("&#60;script&#62;alert(1)&#60;/script&#62; &#38; more"),
        "{html}"
    );
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains(r#"hx-post="/system/update""#), "{html}");
    assert!(html.contains(r#"name="version" value="v0.2.0""#), "{html}");
    assert!(html.contains(r#"class="primary">"#), "{html}");
    assert!(html.contains("Apply update"), "{html}");
    assert!(
        html.contains(r#"hx-confirm="Update hoster to v0.2.0?"#),
        "{html}"
    );
    assert!(
        html.contains("Stacks keep running while hoster restarts"),
        "{html}"
    );
    // The fake release names no date or page.
    assert!(!html.contains("released"), "{html}");
    assert!(!html.contains("full changelog"), "{html}");
    assert!(!html.contains("disabled"), "{html}");
    // Checking applies nothing.
    assert_eq!(host.current(), Some(v("0.1.0")));
    assert!(!shutdown.is_requested());
}

#[tokio::test]
async fn checking_shows_the_release_date_and_the_changelog_link() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let mut source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    source.releases[0].published = Some(humantime::parse_rfc3339("2026-11-06T09:30:00Z").unwrap());
    source.releases[0].page = Some(
        "https://github.com/LHelge/hoster/releases/tag/v0.2.0"
            .parse()
            .unwrap(),
    );
    let (app, _) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    let (_, html) = check(&app, &browser).await;
    assert!(
        html.contains("· released 2026-11-06 · this is v0.1.0"),
        "{html}"
    );
    assert!(
        html.contains(
            r#"href="https://github.com/LHelge/hoster/releases/tag/v0.2.0">full changelog"#
        ),
        "{html}"
    );
}

#[tokio::test]
async fn updating_switches_and_requests_a_graceful_shutdown() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, shutdown) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("v0.1.0 → v0.2.0"), "{html}");
    // The steps the updater did, then the restart.
    assert!(
        html.contains(&format!(
            "downloaded <code>hoster-{TARGET}</code> and verified it against <code>SHA256SUMS</code>"
        )),
        "{html}"
    );
    assert!(html.contains("installed v0.2.0 under"), "{html}");
    assert!(
        html.contains("snapshotted the database to <code>"),
        "{html}"
    );
    assert!(
        html.contains("switched <code>current</code> to v0.2.0"),
        "{html}"
    );
    assert!(
        html.contains(
            r#"<li class="step-now"><span aria-hidden="true">●</span> restarting into v0.2.0"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"hx-get="/system/version?from=v0.1.0" hx-trigger="every 2s""#),
        "{html}"
    );
    assert_eq!(host.current(), Some(v("0.2.0")));
    assert!(
        shutdown.is_requested(),
        "the supervisor is asked to restart"
    );
    // The event log names the signed-in operator who updated (ADR-0055).
    let store = host.store().await;
    let query = crate::store::EventQuery {
        kinds: [crate::store::EventKind::HosterUpdated].into(),
        ..Default::default()
    };
    let events = store.event_page(&query).await.unwrap().events;
    store.close().await;
    let metas: Vec<_> = events.iter().map(|e| e.meta.as_deref()).collect();
    assert_eq!(metas, [Some("v0.1.0 → v0.2.0 · by @octocat")]);
}

#[tokio::test]
async fn the_restart_steps_list_pruned_versions() {
    let host = Host::new(&[("0.0.9", schema()), ("0.1.0", schema())]).await;
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, _) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("removed old v0.0.9</li>"), "{html}");
}

#[tokio::test]
async fn the_restart_steps_say_when_there_was_no_database() {
    let host = Host::new(&[("0.1.0", schema())]).await;
    std::fs::remove_file(host.state_dir.join(Store::FILE_NAME)).unwrap();
    let source = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema()));
    let (app, _) = app_on(&host, source, "0.1.0").await;
    let browser = app.sign_in().await;
    let (status, html) = update(&app, &browser, "v0.2.0").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("no database to snapshot yet"), "{html}");
    assert!(!html.contains("snapshotted the database"), "{html}");
    assert!(!html.contains("removed old"), "{html}");
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

mod log_store {
    use std::collections::BTreeMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::Duration;

    use super::super::logs::FollowerCounts;
    use super::*;
    use crate::logs::{Drops, LogSink};
    use crate::runtime::ContainerId;
    use crate::store::{
        LogBatch, LogRecord, LogStore, Removed, RequestId, RequestRow, RowCounts, Stream,
        SweepReport, Timestamp,
    };

    /// A follower registry with fixed counts.
    struct Followers {
        followed: usize,
        capped: BTreeMap<ContainerId, u64>,
    }

    impl FollowerCounts for Followers {
        fn followed(&self) -> usize {
            self.followed
        }

        fn capped(&self) -> BTreeMap<ContainerId, u64> {
            self.capped.clone()
        }
    }

    fn record(time: Timestamp, stream: Stream) -> LogRecord {
        LogRecord {
            time,
            stack: Some("notes".to_owned()),
            service: "api".to_owned(),
            generation: Some(1),
            stream,
            level: None,
            message: "listening".to_owned(),
            fields: None,
            cut: false,
            marker: false,
        }
    }

    fn request(time: Timestamp) -> RequestRow {
        RequestRow {
            time,
            host: "notes.example.com".to_owned(),
            prefix: "/".to_owned(),
            stack: "notes".to_owned(),
            service: "api".to_owned(),
            method: "GET".to_owned(),
            path: "/".to_owned(),
            status: 200,
            to_header: None,
            total: Duration::from_millis(3),
            bytes: 10,
            client: IpAddr::V4(Ipv4Addr::LOCALHOST),
            upstream_failed: false,
            request_id: RequestId::from([1; 16]),
        }
    }

    /// The System page of an app whose log store card shows `logs`.
    async fn page(logs: Arc<dyn LogFacts>) -> String {
        let host = Host::new(&[("0.1.0", schema())]).await;
        let mut app = TestApp::new().await;
        app.app.system = System::new(
            host.updater(FakeReleases::default(), "0.1.0"),
            ShutdownRequest::default(),
            Arc::new(Facts::sample()),
        )
        .with_logs(logs);
        let browser = app.sign_in().await;
        let response = app.get("/system", Some(&browser.cookie)).await;
        assert_eq!(response.status(), StatusCode::OK);
        body(response).await
    }

    /// The card's markup, from its heading on.
    fn card(html: &str) -> &str {
        let start = html
            .find(r#"<section class="card log-card""#)
            .expect("the card");
        let end = html[start..].find("</section>").unwrap() + start;
        &html[start..end]
    }

    #[tokio::test]
    async fn the_card_shows_size_reach_and_the_last_hour_of_a_seeded_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = LogStore::open(dir.path()).await.unwrap();
        let now = Timestamp::now();
        let oldest = now - Duration::from_secs(2 * 3_600);
        store
            .append(&LogBatch {
                records: vec![
                    record(oldest, Stream::Stdout),
                    record(now - Duration::from_secs(600), Stream::Stdout),
                    record(now - Duration::from_secs(60), Stream::Stderr),
                ],
                requests: vec![request(now - Duration::from_secs(300))],
                ..LogBatch::default()
            })
            .await
            .unwrap();
        let logs = LocalLogs::new(
            LogStoreState::Open(store.clone()),
            LogSink::disconnected(),
            Arc::new(Followers {
                followed: 2,
                capped: BTreeMap::new(),
            }),
            Arc::new(crate::runtime::FakeRuntime::new()),
        );
        let html = page(Arc::new(logs)).await;
        let card = card(&html);
        assert!(
            card.contains(r#"<h2 id="logs-title">Log store</h2>"#),
            "{card}"
        );
        assert!(card.contains(&store.path().display().to_string()), "{card}");
        // Far below the cap: an ok meter, in MiB against the GiB.
        assert!(card.contains(r#"class="meter meter-ok""#), "{card}");
        assert!(card.contains(" MiB / 1.0 GiB"), "{card}");
        let shown = humantime::format_rfc3339_seconds(SystemTime::from(oldest)).to_string();
        assert!(card.contains(&shown), "{card}");
        assert!(card.contains("<dd>2 records · 1 requests</dd>"), "{card}");
        assert!(card.contains("<dd>2 containers</dd>"), "{card}");
        assert!(
            card.contains("7 days <span class=\"host-note\">· no retention pass has finished"),
            "{card}"
        );
        assert!(
            card.contains(r#"<p class="log-losses-none">Nothing.</p>"#),
            "{card}"
        );
        store.close().await;
    }

    #[tokio::test]
    async fn the_card_counts_losses_by_cause_and_names_capped_containers() {
        let dir = tempfile::tempdir().unwrap();
        let store = LogStore::open(dir.path()).await.unwrap();
        // No recorder reads this sink: everything offered is dropped and counted.
        let sink = LogSink::disconnected();
        for _ in 0..3 {
            sink.try_record(record(Timestamp::now(), Stream::Stdout));
        }
        sink.try_record(record(Timestamp::now(), Stream::Hoster));
        let capped = BTreeMap::from([(
            ContainerId::from("0123456789abcdef0123456789abcdef".to_owned()),
            1_240,
        )]);
        let logs = LocalLogs::new(
            LogStoreState::Open(store.clone()),
            sink,
            Arc::new(Followers {
                followed: 1,
                capped,
            }),
            Arc::new(crate::runtime::FakeRuntime::new()),
        );
        let html = page(Arc::new(logs)).await;
        let card = card(&html);
        assert!(
            card.contains("<dt>channel full</dt>\n    <dd>3 container lines <span"),
            "{card}"
        );
        // A container the runtime does not name is shown by its short id.
        assert!(
            card.contains("<dt>line-rate cap</dt>\n    <dd>0123456789ab 1,240 <span"),
            "{card}"
        );
        assert!(card.contains("<dd>1 channel full"), "{card}");
        // Causes that lost nothing are left out.
        assert!(!card.contains("failed commits"), "{card}");
        assert!(!card.contains("Nothing."), "{card}");
        store.close().await;
    }

    #[tokio::test]
    async fn the_card_says_when_the_size_cap_decides_and_lists_the_top_capped() {
        let now = SystemTime::now();
        let mut figures = LogFigures::sample(now);
        figures.used_bytes = Some(1_000 * 1024 * 1024);
        figures.sweep = Some(SweepReport {
            started: Timestamp::from(now),
            expired: Removed::default(),
            capped: Removed {
                records: 52_000,
                requests: 1_200,
            },
            file_bytes: 1 << 30,
        });
        figures.drops = Drops {
            failed_commits: 2,
            lost_in_failed_commits: 731,
            ..Drops::default()
        };
        figures.hoster_capped = 7;
        figures.last_hour = Some(RowCounts {
            records: 1_204_000,
            requests: 0,
        });
        let html = page(Arc::new(LogHealth::Open(Box::new(figures)))).await;
        let card = card(&html);
        // 1000 MiB of 1024: past 95 %.
        assert!(card.contains(r#"class="meter meter-fail""#), "{card}");
        assert!(card.contains("1000 MiB / 1.0 GiB"), "{card}");
        assert!(card.contains("98% of the cap"), "{card}");
        assert!(
            card.contains("the 1.0 GiB size cap <span class=\"host-note\">· the last pass removed 52,000 records and 1,200 requests"),
            "{card}"
        );
        assert!(
            card.contains("<dd>1,204,000 records · 0 requests</dd>"),
            "{card}"
        );
        assert!(
            card.contains("<dd>2 commits · 731 records and rows"),
            "{card}"
        );
        assert!(
            card.contains("notes-api-7 12,040 · blog-app-3 310 · shop-web-2 22 · +1 more (5)"),
            "{card}"
        );
        assert!(card.contains("<dd>7 rate cap"), "{card}");
        assert!(!card.contains("channel full"), "{card}");
    }

    #[tokio::test]
    async fn an_unopened_store_is_reported_and_the_rest_of_the_page_works() {
        let logs = LocalLogs::new(
            LogStoreState::Unavailable(
                "cannot open /srv/hoster/logs.db: database disk image is malformed".into(),
            ),
            LogSink::disconnected(),
            Arc::new(Followers {
                followed: 0,
                capped: BTreeMap::new(),
            }),
            Arc::new(crate::runtime::FakeRuntime::new()),
        );
        let html = page(Arc::new(logs)).await;
        let card = card(&html);
        assert!(
            card.contains("could not be opened, so logs are not persisted: cannot open /srv/hoster/logs.db: database disk image is malformed."),
            "{card}"
        );
        assert!(!card.contains("<dt>size</dt>"), "{card}");
        assert!(
            html.contains(r#"<span class="version-now">v0.1.0</span>"#),
            "{html}"
        );
        assert!(html.contains(r#"<dl class="host-facts">"#), "{html}");
    }

    #[tokio::test]
    async fn without_log_figures_the_card_is_left_out() {
        let host = Host::new(&[("0.1.0", schema())]).await;
        let (app, _) = app_on(&host, FakeReleases::default(), "0.1.0").await;
        let browser = app.sign_in().await;
        let html = body(app.get("/system", Some(&browser.cookie)).await).await;
        assert!(!html.contains("log-card"), "{html}");
    }
}
