use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use super::*;
use crate::web::auth::tests::TestApp;

async fn get(path: &str) -> Response {
    TestApp::new()
        .await
        .app
        .router()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn app() -> WebApp {
    TestApp::new().await.app
}

async fn body(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn header<'a>(response: &'a Response, name: &str) -> &'a str {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("{name} is set"))
        .to_str()
        .unwrap()
}

#[tokio::test]
async fn healthz_answers_ok_without_a_session() {
    let response = get("/healthz").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "cache-control"), "no-store");
    assert_eq!(body(response).await, "ok\n");
}

#[tokio::test]
async fn stack_schema_is_served_without_a_session() {
    let response = get("/schema/stack-v1.json").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "content-type"), "application/schema+json");
    assert_eq!(header(&response, "cache-control"), "public, max-age=3600");
    let schema: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
    assert_eq!(schema["$id"], StackSchema::URL);
    assert_eq!(schema["title"], "hoster stack file");
}

#[tokio::test]
async fn every_response_carries_the_security_headers() {
    for path in [
        "/",
        "/healthz",
        "/no-such-page",
        "/assets/missing.js",
        StackSchema::ADMIN_PATH,
    ] {
        let response = get(path).await;
        let csp = header(&response, "content-security-policy");
        assert!(csp.starts_with("default-src 'self';"), "{path}: {csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");
        assert!(!csp.contains("unsafe-"), "{path}: {csp}");
        assert_eq!(header(&response, "x-content-type-options"), "nosniff");
        assert_eq!(header(&response, "referrer-policy"), "same-origin");
        assert_eq!(header(&response, "x-frame-options"), "DENY");
    }
}

#[tokio::test]
async fn dashboard_renders_the_layout() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app.get("/", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header(&response, "content-type").starts_with("text/html"));
    let html = body(response).await;
    assert!(html.contains("<h1>Dashboard</h1>"));
    assert!(html.contains(&format!(
        r#"<script src="{}" defer></script>"#,
        Assets::get().htmx.url()
    )));
}

/// The in-process statistics of ADR-0045 on the dashboard, read through the `Stats` handle.
#[tokio::test]
async fn dashboard_shows_the_statistics() {
    use std::time::{Duration, SystemTime};

    use crate::routing::{Owner, RouteEntry, RouteOwner, Target};
    use crate::stack::RouteTimeouts;
    use crate::stats::{CertExpiry, Histogram, RequestOutcome};
    use crate::store::{
        DeploymentOutcome, DeploymentState, Resolution, Timestamp, TrafficBucket, TrafficCounts,
        TrafficSeries,
    };

    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = body(app.get("/", Some(&browser.cookie)).await).await;
    assert!(html.contains("no requests"), "{html}");

    *app.stacks.stacks.lock().unwrap() = vec![crate::web::stacks::tests::notes_status()];
    let route = RouteEntry {
        owner: Owner::Route(RouteOwner {
            stack: "notes".parse().unwrap(),
            service: "api".parse().unwrap(),
        }),
        prefix: "/".parse().unwrap(),
        prefix_str: "/".into(),
        strip_prefix: false,
        target: Target::Unavailable {
            retry_after: Duration::from_secs(1),
        },
        timeouts: RouteTimeouts::default(),
    };
    // Three requests in the history, two minutes ago; one finished just now, not flushed yet.
    let mut latency = Histogram::default();
    for _ in 0..3 {
        latency.record(Duration::from_millis(20));
    }
    app.stats.traffic.lock().unwrap().push(TrafficBucket {
        start: Resolution::Minute.bucket(Timestamp::now()) - Duration::from_secs(120),
        series: TrafficSeries {
            host: "notes.example.com".into(),
            prefix: "/".into(),
            stack: "notes".into(),
            service: "api".into(),
        },
        counts: TrafficCounts {
            s2xx: 2,
            s5xx: 1,
            upstream_errors: 1,
            ..TrafficCounts::default()
        },
        latency,
    });
    app.traffic
        .route("notes.example.com", &route)
        .finish(RequestOutcome {
            status: Some(200),
            latency: Duration::from_millis(20),
            upstream_error: false,
        });
    let _in_flight = app.traffic.route("notes.example.com", &route);
    app.stats.deployments.lock().unwrap().extend([
        DeploymentOutcome {
            stack: "notes".into(),
            state: DeploymentState::Failed,
            count: 2,
            mean: Duration::from_secs(5),
            max: Duration::from_secs(9),
        },
        DeploymentOutcome {
            stack: "notes".into(),
            state: DeploymentState::Live,
            count: 7,
            mean: Duration::from_secs(12),
            max: Duration::from_secs(40),
        },
    ]);
    app.stats.certificates.lock().unwrap().push(CertExpiry {
        host: "notes.example.com".into(),
        not_after: SystemTime::now() + Duration::from_secs(3 * 86_400 + 60),
    });

    let html = body(app.get("/", Some(&browser.cookie)).await).await;
    let compact: String = html.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        compact.contains(r#"<dt class="kpi-label">requests · 24h</dt><dd class="kpi-value">3</dd><dd class="kpi-sub">no previous data · 1 in flight</dd>"#),
        "{compact}"
    );
    assert!(
        compact.contains(r#"<dt class="kpi-label">5xx rate</dt><dd class="kpi-value">33.3 %</dd><dd class="kpi-sub">1 of 3</dd>"#),
        "{compact}"
    );
    assert!(
        compact.contains(r#"<td class="num stat-fail">33.3 %</td>"#),
        "{compact}"
    );
    assert!(
        compact.contains(r#"7 live · <span class="stat-fail">2 failed</span></span><span class="dash-sub">mean 12.0 s · max 40.0 s</span>"#),
        "{compact}"
    );
    assert!(
        compact.contains(&format!(
            "<span class=\"pill pill-warn\">{}certificate expiring</span> <a href=\"/certificates\">notes.example.com</a>",
            crate::web::Icon::Warning
        )),
        "{compact}"
    );
    // 3 requests in the last 5 minutes.
    assert!(compact.contains(r#"<td class="num">0.6</td>"#), "{compact}");
    assert!(!compact.contains("since"), "{compact}");
    assert!(!compact.contains("Cannot show"), "{compact}");
}

#[tokio::test]
async fn htmx_is_served_with_a_long_cache_and_javascript_type() {
    let url = Assets::get().htmx.url();
    let response = get(&url).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "content-type"),
        "text/javascript; charset=utf-8"
    );
    assert_eq!(
        header(&response, "cache-control"),
        "public, max-age=31536000, immutable"
    );
    assert!(body(response).await.starts_with("var htmx="));
}

#[tokio::test]
async fn fonts_are_served_as_woff2_with_a_long_cache_under_the_csp() {
    for font in &Assets::get().fonts {
        let response = get(&font.url()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "content-type"), "font/woff2");
        assert_eq!(
            header(&response, "cache-control"),
            "public, max-age=31536000, immutable"
        );
        // Same-origin, so `default-src 'self'` covers it with no font-src of its own.
        let csp = header(&response, "content-security-policy");
        assert!(csp.starts_with("default-src 'self';"), "{csp}");
        assert!(!csp.contains("font-src"), "{csp}");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.starts_with(b"wOF2"));
    }
}

#[tokio::test]
async fn icon_sprite_is_served_as_svg_with_a_long_cache() {
    let url = Assets::get().icons.url();
    let response = get(&url).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "content-type"),
        "image/svg+xml; charset=utf-8"
    );
    assert_eq!(
        header(&response, "cache-control"),
        "public, max-age=31536000, immutable"
    );
    // An SVG opened directly is a document: it gets the same CSP and no sniffing.
    assert!(header(&response, "content-security-policy").starts_with("default-src 'self';"));
    assert_eq!(header(&response, "x-content-type-options"), "nosniff");
    let svg = body(response).await;
    assert!(svg.contains(r#"<symbol id="redeploy" "#), "{svg}");

    // Only the hashed name is served, so a cached page never pairs with another sprite.
    assert_eq!(
        get("/assets/icons.svg").await.status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn stylesheet_is_served_as_css() {
    let response = get(&Assets::get().stylesheet.url()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "content-type"), "text/css; charset=utf-8");
}

#[tokio::test]
async fn unhashed_asset_names_are_not_found() {
    assert_eq!(
        get("/assets/htmx.min.js").await.status(),
        StatusCode::NOT_FOUND
    );
}

/// Sends a bare HTTP/1.1 GET over the Unix socket at `path`.
fn get_over_socket(path: &Path, target: &str) -> String {
    let mut stream = std::os::unix::net::UnixStream::connect(path).unwrap();
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: hoster.example.com\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[tokio::test]
async fn serves_on_a_0600_unix_socket_until_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(Config::ADMIN_SOCKET_NAME);
    let token = CancellationToken::new();
    let mut server = app()
        .await
        .start(Listen::Unix(path.clone()), token.child_token())
        .await
        .unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let socket = path.clone();
    let response = tokio::task::spawn_blocking(move || get_over_socket(&socket, "/healthz"))
        .await
        .unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("ok\n"), "{response}");

    token.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), server.stop(true))
        .await
        .expect("graceful shutdown finishes")
        .unwrap();
    assert!(!path.exists(), "the socket is removed after stopping");
}

#[tokio::test]
async fn replaces_a_stale_socket_but_not_a_live_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(Config::ADMIN_SOCKET_NAME);
    // A socket file nobody accepts on, as left behind by a crash.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let mut server = app()
        .await
        .start(Listen::Unix(path.clone()), CancellationToken::new())
        .await
        .expect("a stale socket is replaced");

    let err = app()
        .await
        .start(Listen::Unix(path.clone()), CancellationToken::new())
        .await
        .err()
        .expect("a live socket is not taken over");
    assert!(format!("{err:#}").contains("already running"), "{err:#}");

    server.stop(false).await.unwrap();
}

#[tokio::test]
async fn keeps_a_socket_it_cannot_probe() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(Config::ADMIN_SOCKET_NAME);
    // A live socket we may not connect to: the probe fails with "permission denied", not
    // "connection refused", so it must not be mistaken for a stale one.
    let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        // Running as root: permissions do not apply, so this case cannot be set up.
        return;
    }
    let err = app()
        .await
        .start(Listen::Unix(path.clone()), CancellationToken::new())
        .await
        .err()
        .expect("a socket that cannot be probed is left alone");
    assert!(format!("{err:#}").contains("stale"), "{err:#}");
    assert!(path.exists(), "the live socket is kept");
    drop(live);
}

#[tokio::test]
async fn refuses_to_replace_a_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(Config::ADMIN_SOCKET_NAME);
    std::fs::write(&path, "not a socket").unwrap();
    let err = app()
        .await
        .start(Listen::Unix(path.clone()), CancellationToken::new())
        .await
        .err()
        .expect("a regular file is left alone");
    assert!(format!("{err:#}").contains("not a socket"), "{err:#}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a socket");
}

#[tokio::test]
async fn serves_on_loopback_tcp_for_development() {
    let mut server = app()
        .await
        .start(
            Listen::Tcp("127.0.0.1:0".parse().unwrap()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    server.stop(true).await.unwrap();
}

mod containers {
    use std::time::Duration;

    use super::*;
    use crate::runtime::{
        ContainerId, ContainerRuntime, ContainerSpec, LogLine, LogStream, NetworkSpec,
        ServiceGeneration, label,
    };
    use crate::store::DeploymentId;
    use crate::web::auth::tests::Browser;

    const IMAGE: &str =
        "ghcr.io/me/notes@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// A signed-in app with one running container, `hoster-notes-web-g3`, that has logged two
    /// lines.
    async fn with_container() -> (TestApp, Browser, ContainerId) {
        let app = TestApp::new().await;
        let browser = app.sign_in().await;
        let generation = ServiceGeneration {
            stack: "notes".parse().unwrap(),
            service: "web".parse().unwrap(),
            generation: 3,
        };
        let spec = ContainerSpec::new(&generation, DeploymentId::from(1), IMAGE.parse().unwrap());
        let runtime = &app.runtime;
        runtime
            .ensure_network(&NetworkSpec::for_stack(&generation.stack))
            .await
            .unwrap();
        runtime.pull_image(&spec.image).await.unwrap();
        let id = runtime.create_container(&spec).await.unwrap();
        runtime.start(&id).await.unwrap();
        runtime.push_log(&id, line(LogStream::Stdout, "listening on :8080"));
        runtime.push_log(&id, line(LogStream::Stderr, "warning: <no config>"));
        (app, browser, id)
    }

    fn line(stream: LogStream, text: &str) -> LogLine {
        LogLine {
            stream,
            time: Some(humantime::parse_rfc3339("2026-11-08T12:00:00Z").unwrap()),
            text: text.into(),
        }
    }

    /// Reads body frames until `needle` has arrived, and returns everything read so far.
    async fn read_until(body: &mut Body, needle: &str) -> String {
        let mut text = String::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !text.contains(needle) {
                let frame = body.frame().await.expect("the stream is open").unwrap();
                if let Ok(data) = frame.into_data() {
                    text.push_str(std::str::from_utf8(&data).unwrap());
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("`{needle}` never arrived"));
        text
    }

    #[tokio::test]
    async fn containers_are_listed_on_their_stack_and_their_log_links_back() {
        let (app, browser, _) = with_container().await;
        // The container list moved to the stacks' pages (ADR-0054).
        let response = app.get("/containers", Some(&browser.cookie)).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/stacks");
        // `web` is not a service of the fake's `notes`, so it is listed after the services.
        let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
        assert!(
            html.contains(r#"<code class="service-container">hoster-notes-web-g3</code>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<td>service not in the active revision</td>"#),
            "{html}"
        );
        assert!(
            html.contains(
                r#"href="/stacks/notes/logs?service=web&#38;container=hoster-notes-web-g3""#
            ),
            "{html}"
        );

        // The old log URL leads to the stack's logs tab, with the same options.
        for (url, location) in [
            (
                "/containers/hoster-notes-web-g3/logs",
                "/stacks/notes/logs?service=web&container=hoster-notes-web-g3",
            ),
            (
                "/containers/hoster-notes-web-g3/logs?tail=10&since=1h&follow=on",
                "/stacks/notes/logs?service=web&container=hoster-notes-web-g3&tail=10&since=1h&follow=on",
            ),
        ] {
            let response = app.get(url, Some(&browser.cookie)).await;
            assert_eq!(response.status(), StatusCode::SEE_OTHER, "{url}");
            assert_eq!(response.headers()[header::LOCATION], location, "{url}");
        }

        // The tab belongs to Stacks and shows the container's log under the stack's header.
        let html = body(
            app.get(
                "/stacks/notes/logs?service=web&container=hoster-notes-web-g3",
                Some(&browser.cookie),
            )
            .await,
        )
        .await;
        assert!(
            html.contains(r#"<li><a href="/stacks" aria-current="page">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<nav class="crumb" aria-label="Breadcrumb"><a href="/stacks">~/stacks</a>/<a href="/stacks/notes">notes</a>/logs</nav>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<a role="tab" href="/stacks/notes/logs" aria-selected="true" aria-current="page">logs</a>"#),
            "{html}"
        );
        // `web` is not in the active revision, but its container has a chip after the services.
        assert!(
            html.contains(r#"<a class="log-chip" href="/stacks/notes/logs?service=web&#38;tail=200&#38;since=&#38;follow=on" aria-current="page"><span class="dot dot-s2" aria-hidden="true"></span>web</a>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<input type="hidden" name="container" value="hoster-notes-web-g3">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"hx-sse:connect="/containers/hoster-notes-web-g3/logs/stream?tail=200&#38;since=&#38;follow=on""#),
            "{html}"
        );
        assert!(!html.contains("style="), "{html}");
    }

    #[tokio::test]
    async fn containers_of_removed_stacks_are_listed_on_the_stacks_page() {
        let (app, browser, _) = with_container().await;
        let html = body(app.get("/stacks", Some(&browser.cookie)).await).await;
        assert!(!html.contains("Containers of removed stacks"), "{html}");

        // A container left behind by a stack hoster no longer has.
        let generation = ServiceGeneration {
            stack: "gone".parse().unwrap(),
            service: "web".parse().unwrap(),
            generation: 1,
        };
        let spec = ContainerSpec::new(&generation, DeploymentId::from(1), IMAGE.parse().unwrap());
        let runtime = &app.runtime;
        runtime
            .ensure_network(&NetworkSpec::for_stack(&generation.stack))
            .await
            .unwrap();
        let id = runtime.create_container(&spec).await.unwrap();
        runtime.start(&id).await.unwrap();

        let html = body(app.get("/stacks", Some(&browser.cookie)).await).await;
        assert!(html.contains("Containers of removed stacks"), "{html}");
        assert!(
            html.contains(r#"<td><code>hoster-gone-web-g1</code></td>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"href="/containers/hoster-gone-web-g1/logs""#),
            "{html}"
        );
        assert!(
            !html.contains("<td><code>hoster-notes-web-g3</code></td>"),
            "notes is a stack hoster has: {html}"
        );
        assert!(html.contains(r#"<section id="leftovers""#), "{html}");

        // Its log leads back to that list, not to the stack that is gone.
        let html = body(
            app.get("/containers/hoster-gone-web-g1/logs", Some(&browser.cookie))
                .await,
        )
        .await;
        assert!(!html.contains(r#"href="/stacks/gone"#), "{html}");
        assert!(
            html.contains(r#"<nav class="crumb" aria-label="Breadcrumb"><a href="/stacks">~/stacks</a>/<a href="/stacks#leftovers">removed</a>/logs/hoster-gone-web-g1</nav>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<a href="/stacks#leftovers">Containers of removed stacks</a>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<form class="log-options" method="get" action="/containers/hoster-gone-web-g1/logs">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"hx-sse:connect="/containers/hoster-gone-web-g1/logs/stream?"#),
            "{html}"
        );
    }

    #[tokio::test]
    async fn log_page_connects_to_the_stream_with_its_query() {
        let (app, browser, _) = with_container().await;
        let response = app
            .get(
                "/stacks/notes/logs?service=web&tail=10&since=1h&follow=on",
                Some(&browser.cookie),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = body(response).await;
        assert!(
            html.contains(
                r#"hx-sse:connect="/containers/hoster-notes-web-g3/logs/stream?tail=10&#38;since=1h&#38;follow=on""#
            ),
            "{html}"
        );
        assert!(html.contains(r#"hx-sse:close="end""#), "{html}");
        assert!(html.contains(r#"name="tail" min="0" value="10""#), "{html}");
        assert!(html.contains(r#"name="since" value="1h""#), "{html}");
        assert!(
            html.contains(r#"<a class="log-preset" href="/stacks/notes/logs?service=web&#38;tail=10&#38;since=1h&#38;follow=on" aria-current="true">1h</a>"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<input type="checkbox" role="switch" name="follow" checked>"#),
            "{html}"
        );
        assert!(html.contains(">following</span>"), "{html}");
        // The chosen service is kept by the form; no container was named.
        assert!(
            html.contains(r#"<input type="hidden" name="service" value="web">"#),
            "{html}"
        );
        assert!(!html.contains(r#"name="container""#), "{html}");
        assert!(
            html.contains(&format!(
                r#"<script src="{}" defer></script>"#,
                Assets::get().htmx_sse.url()
            )),
            "the SSE extension is loaded: {html}"
        );
    }

    #[tokio::test]
    async fn invalid_log_options_are_reported_without_a_stream() {
        let (app, browser, _) = with_container().await;
        let page = "/stacks/notes/logs?service=web&tail=lots&since=";
        let html = body(app.get(page, Some(&browser.cookie)).await).await;
        assert!(html.contains("flash-error"), "{html}");
        assert!(html.contains("whole number"), "{html}");
        assert!(!html.contains("hx-sse:connect"), "{html}");

        let stream = "/containers/hoster-notes-web-g3/logs/stream?since=soon";
        let response = app.get(stream, Some(&browser.cookie)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_and_unmanaged_containers_are_not_found() {
        let (app, browser, _) = with_container().await;
        // A container hoster did not create, under a name it could have.
        let generation = ServiceGeneration {
            stack: "notes".parse().unwrap(),
            service: "foreign".parse().unwrap(),
            generation: 1,
        };
        let mut spec =
            ContainerSpec::new(&generation, DeploymentId::from(1), IMAGE.parse().unwrap());
        spec.labels.remove(label::MANAGED);
        app.runtime.create_container(&spec).await.unwrap();

        for name in ["hoster-notes-web-g9", "hoster-notes-foreign-g1"] {
            for path in [
                format!("/containers/{name}/logs"),
                format!("/containers/{name}/logs/stream"),
            ] {
                let response = app.get(&path, Some(&browser.cookie)).await;
                assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            }
        }
    }

    #[tokio::test]
    async fn logs_need_a_session() {
        let (app, _, _) = with_container().await;
        let response = app
            .get("/containers/hoster-notes-web-g3/logs/stream", None)
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    #[tokio::test]
    async fn unfollowed_log_streams_the_lines_then_ends() {
        let (app, browser, _) = with_container().await;
        let response = app
            .get(
                "/containers/hoster-notes-web-g3/logs/stream?tail=&since=",
                Some(&browser.cookie),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, "content-type"), "text/event-stream");
        let text = body(response).await;
        // Both lines in one message, stdout and stderr told apart, the text escaped.
        assert!(
            text.contains(
                r#"<span class="log-line log-stdout"><time datetime="2026-11-08T12:00:00.000Z">2026-11-08T12:00:00.000Z</time> listening on :8080</span>"#
            ),
            "{text}"
        );
        assert!(
            text.contains(
                r#"<span class="log-line log-stderr" title="stderr"><span class="visually-hidden">stderr: </span>"#
            ),
            "{text}"
        );
        assert!(text.contains("warning: &#60;no config&#62;"), "{text}");
        // The batch carries its last line's time, for resuming.
        assert!(text.contains("\nid: 1794139200000000000\n"), "{text}");
        assert!(text.contains("End of the log."), "{text}");
        assert!(text.trim_end().ends_with("\nevent: end"), "{text:?}");
    }

    #[tokio::test]
    async fn tail_limits_the_lines() {
        let (app, browser, _) = with_container().await;
        let page = "/containers/hoster-notes-web-g3/logs/stream?tail=1&since=";
        let text = body(app.get(page, Some(&browser.cookie)).await).await;
        assert!(!text.contains("listening"), "{text}");
        assert!(text.contains("no config"), "{text}");
    }

    #[tokio::test]
    async fn reconnects_resume_after_the_last_event_id() {
        let (app, browser, id) = with_container().await;
        let later = LogLine {
            time: Some(humantime::parse_rfc3339("2026-11-08T12:00:01Z").unwrap()),
            ..line(LogStream::Stdout, "request served")
        };
        app.runtime.push_log(&id, later);
        let request = Request::get("/containers/hoster-notes-web-g3/logs/stream?tail=1")
            .header(header::COOKIE, &browser.cookie)
            .header("last-event-id", "1794139200000000000")
            .body(Body::empty())
            .unwrap();
        let text = body(app.send(request).await).await;
        assert!(!text.contains("listening"), "{text}");
        assert!(!text.contains("no config"), "{text}");
        assert!(text.contains("request served"), "{text}");
    }

    #[tokio::test]
    async fn followed_log_streams_new_lines_and_stops_when_the_browser_leaves() {
        let (app, browser, id) = with_container().await;
        let response = app
            .get(
                "/containers/hoster-notes-web-g3/logs/stream",
                Some(&browser.cookie),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body();
        read_until(&mut stream, "no config").await;
        assert_eq!(app.runtime.log_followers(&id), 1);

        app.runtime
            .push_log(&id, line(LogStream::Stdout, "GET /healthz 200"));
        read_until(&mut stream, "GET /healthz 200").await;

        // The browser goes away: the response body is dropped, and the log is no longer read.
        drop(stream);
        assert_eq!(app.runtime.log_followers(&id), 0);
    }

    #[tokio::test]
    async fn followed_log_ends_when_the_session_is_revoked() {
        let (app, browser, id) = with_container().await;
        let response = app
            .get(
                "/containers/hoster-notes-web-g3/logs/stream",
                Some(&browser.cookie),
            )
            .await;
        let mut stream = response.into_body();
        read_until(&mut stream, "no config").await;
        // "Log out everywhere", e.g. from another device.
        app.store.remove_all_sessions().await.unwrap();
        let text = read_until(&mut stream, "event: end").await;
        assert!(text.contains("Signed out"), "{text}");
        assert!(stream.frame().await.is_none(), "the stream ends");
        assert_eq!(
            app.runtime.log_followers(&id),
            0,
            "podman is no longer read"
        );
    }

    #[tokio::test]
    async fn followed_log_ends_when_the_container_stops() {
        let (app, browser, id) = with_container().await;
        let response = app
            .get(
                "/containers/hoster-notes-web-g3/logs/stream",
                Some(&browser.cookie),
            )
            .await;
        let mut stream = response.into_body();
        read_until(&mut stream, "no config").await;
        app.runtime.stop(&id, Duration::ZERO).await.unwrap();
        read_until(&mut stream, "event: end").await;
        assert!(stream.frame().await.is_none(), "the stream ends");
    }
}
