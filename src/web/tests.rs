use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use super::*;

async fn get(path: &str) -> Response {
    WebApp::router()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
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
async fn every_response_carries_the_security_headers() {
    for path in ["/", "/healthz", "/no-such-page", "/assets/missing.js"] {
        let response = get(path).await;
        let csp = header(&response, "content-security-policy");
        assert!(csp.starts_with("default-src 'self';"), "{path}: {csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");
        assert!(!csp.contains("unsafe-"), "{path}: {csp}");
        assert_eq!(header(&response, "x-content-type-options"), "nosniff");
        assert_eq!(header(&response, "referrer-policy"), "no-referrer");
        assert_eq!(header(&response, "x-frame-options"), "DENY");
    }
}

#[tokio::test]
async fn dashboard_renders_the_layout() {
    let response = get("/").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header(&response, "content-type").starts_with("text/html"));
    let html = body(response).await;
    assert!(html.contains("<h1>Dashboard</h1>"));
    assert!(html.contains(&format!(
        r#"<script src="{}" defer></script>"#,
        Assets::get().htmx.url()
    )));
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
    let path = dir.path().join(Listen::SOCKET_NAME);
    let token = CancellationToken::new();
    let mut server = WebApp::start(Listen::Unix(path.clone()), token.child_token())
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
    let path = dir.path().join(Listen::SOCKET_NAME);
    // A socket file nobody accepts on, as left behind by a crash.
    drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
    let mut server = WebApp::start(Listen::Unix(path.clone()), CancellationToken::new())
        .await
        .expect("a stale socket is replaced");

    let err = WebApp::start(Listen::Unix(path.clone()), CancellationToken::new())
        .await
        .err()
        .expect("a live socket is not taken over");
    assert!(format!("{err:#}").contains("already running"), "{err:#}");

    server.stop(false).await.unwrap();
}

#[tokio::test]
async fn keeps_a_socket_it_cannot_probe() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(Listen::SOCKET_NAME);
    // A live socket we may not connect to: the probe fails with "permission denied", not
    // "connection refused", so it must not be mistaken for a stale one.
    let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        // Running as root: permissions do not apply, so this case cannot be set up.
        return;
    }
    let err = WebApp::start(Listen::Unix(path.clone()), CancellationToken::new())
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
    let path = dir.path().join(Listen::SOCKET_NAME);
    std::fs::write(&path, "not a socket").unwrap();
    let err = WebApp::start(Listen::Unix(path.clone()), CancellationToken::new())
        .await
        .err()
        .expect("a regular file is left alone");
    assert!(format!("{err:#}").contains("not a socket"), "{err:#}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a socket");
}

#[tokio::test]
async fn serves_on_loopback_tcp_for_development() {
    let mut server = WebApp::start(
        Listen::Tcp("127.0.0.1:0".parse().unwrap()),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    server.stop(true).await.unwrap();
}
