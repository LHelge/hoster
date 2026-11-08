//! The login flow against a mock GitHub (wiremock), sessions and CSRF.

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt as _;
use oauth2::url::Url;
use tempfile::TempDir;
use tower::ServiceExt as _;
use wiremock::matchers::{body_string_contains, header as has_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::runtime::FakeRuntime;
use crate::web::WebApp;
use crate::web::stacks::tests::FakeStacks;

pub const ORIGIN: &str = "https://hoster.example.com";
const TOKEN_PATH: &str = "/login/oauth/access_token";
const USER_PATH: &str = "/api/user";

/// The admin app wired to a mock GitHub, with its own state store.
pub struct TestApp {
    pub github: MockServer,
    pub store: Store,
    /// The containers the app shows.
    pub runtime: Arc<FakeRuntime>,
    /// The orchestrator the stack pages talk to.
    pub stacks: Arc<FakeStacks>,
    pub app: WebApp,
    _dir: TempDir,
}

/// A signed-in browser: the `Cookie` header value and the CSRF token.
pub struct Browser {
    pub cookie: String,
    pub csrf: String,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with_idle_timeout(Duration::from_secs(3600)).await
    }

    async fn with_idle_timeout(idle_timeout: Duration) -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("state")).await.unwrap();
        let github = MockServer::start().await;
        let base = Url::parse(&github.uri()).unwrap();
        let endpoints = GithubEndpoints {
            authorize: base.join("/login/oauth/authorize").unwrap(),
            token: base.join(TOKEN_PATH).unwrap(),
            api: base.join("/api/").unwrap(),
        };
        let client = GithubOAuth::new(
            GithubApp {
                client_id: "test-client".into(),
                client_secret: "test-secret".into(),
            },
            &format!("{ORIGIN}/auth/github/callback"),
            endpoints,
        )
        .unwrap();
        let settings = AuthSettings {
            admin: "OctoCat".parse().unwrap(),
            origin: ORIGIN.into(),
            idle_timeout,
        };
        let runtime = Arc::new(FakeRuntime::new());
        let stacks = Arc::new(FakeStacks::default());
        let app = WebApp::new(
            Auth::new(settings, store.clone(), client),
            runtime.clone(),
            stacks.clone(),
        );
        TestApp {
            github,
            store,
            runtime,
            stacks,
            app,
            _dir: dir,
        }
    }

    pub async fn send(&self, request: Request<Body>) -> Response {
        self.app.router().oneshot(request).await.unwrap()
    }

    pub async fn get(&self, uri: &str, cookie: Option<&str>) -> Response {
        let mut request = Request::get(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    /// A session created directly in the store, as a completed login would.
    pub async fn sign_in(&self) -> Browser {
        self.sign_in_at(Timestamp::now()).await
    }

    async fn sign_in_at(&self, at: Timestamp) -> Browser {
        let token = RandomToken::generate();
        let csrf: String = RandomToken::generate().into();
        self.store
            .create_session(&SessionRecord {
                token_hash: RandomToken::hash(token.as_ref()),
                csrf_token: csrf.clone(),
                github_login: "octocat".into(),
                github_id: 1,
                created_at: at,
                last_seen_at: at,
                expires_at: at + WebConfig::SESSION_LIFETIME,
            })
            .await
            .unwrap();
        Browser {
            cookie: format!("{}={}", Cookie::SESSION, token.as_ref()),
            csrf,
        }
    }

    /// GitHub accepts `code` once and says it belongs to `login`.
    async fn github_accepts(&self, code: &str, login: &str) {
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .and(body_string_contains(format!("code={code}")))
            .and(body_string_contains("code_verifier="))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "gho_test_token",
                "token_type": "bearer",
                "scope": "",
            })))
            .up_to_n_times(1)
            .with_priority(1)
            .named("token exchange")
            .mount(&self.github)
            .await;
        // What GitHub answers for a code it has seen before: 200 with an error body.
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": "bad_verification_code",
                "error_description": "The code passed is incorrect or expired.",
            })))
            .with_priority(5)
            .mount(&self.github)
            .await;
        Mock::given(method("GET"))
            .and(path(USER_PATH))
            .and(has_header("authorization", "Bearer gho_test_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "login": login, "id": 583231 })),
            )
            .mount(&self.github)
            .await;
    }

    /// Starts a login; returns the `state` sent to GitHub and the login cookie.
    async fn start_login(&self) -> (String, String) {
        let response = self.get("/auth/github/login", None).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = Url::parse(header(&response, "location")).unwrap();
        let query = |key: &str| {
            location
                .query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };
        assert_eq!(location.path(), "/login/oauth/authorize");
        assert_eq!(query("client_id").as_deref(), Some("test-client"));
        assert_eq!(query("code_challenge_method").as_deref(), Some("S256"));
        assert!(query("code_challenge").is_some());
        assert_eq!(
            query("redirect_uri").as_deref(),
            Some("https://hoster.example.com/auth/github/callback")
        );
        assert_eq!(query("scope"), None, "no scopes are requested");
        let state = query("state").unwrap();
        let cookie = set_cookie(&response, Cookie::LOGIN).unwrap();
        assert!(cookie.contains("Secure") && cookie.contains("HttpOnly"));
        let cookie = format!("{}={state}", Cookie::LOGIN);
        (state, cookie)
    }

    async fn callback(&self, code: &str, state: &str, cookie: Option<&str>) -> Response {
        self.get(
            &format!("/auth/github/callback?code={code}&state={state}"),
            cookie,
        )
        .await
    }

    async fn session_count(&self) -> i64 {
        let now = Timestamp::now();
        // Through the public API: prune nothing live, then count by removing.
        self.store
            .prune_sessions(now, now - Duration::from_secs(3600))
            .await
            .unwrap();
        self.store.remove_all_sessions().await.unwrap() as i64
    }
}

pub fn header<'a>(response: &'a Response, name: &str) -> &'a str {
    response
        .headers()
        .get(name)
        .unwrap_or_else(|| panic!("{name} is set"))
        .to_str()
        .unwrap()
}

/// The `Set-Cookie` header that sets cookie `name`, if any.
fn set_cookie(response: &Response, name: &str) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .find(|v| v.starts_with(&format!("{name}=")))
}

async fn body(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn post(uri: &str) -> axum::http::request::Builder {
    Request::post(uri)
}

#[tokio::test]
async fn login_succeeds_for_the_admin_case_insensitively() {
    let app = TestApp::new().await;
    app.github_accepts("good-code", "octocat").await;
    let (state, login_cookie) = app.start_login().await;

    let response = app.callback("good-code", &state, Some(&login_cookie)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(header(&response, "location"), "/");
    let session = set_cookie(&response, Cookie::SESSION).expect("a session cookie is set");
    for attribute in [
        "Path=/",
        "Secure",
        "HttpOnly",
        "SameSite=Lax",
        "Max-Age=43200",
    ] {
        assert!(session.contains(attribute), "{session}");
    }
    let cleared = set_cookie(&response, Cookie::LOGIN).unwrap();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");

    let token = session.split(';').next().unwrap();
    let dashboard = app.get("/", Some(token)).await;
    assert_eq!(dashboard.status(), StatusCode::OK);
    let html = body(dashboard).await;
    assert!(html.contains("<h1>Dashboard</h1>"));
    assert!(html.contains("octocat"));
    assert!(html.contains("hx-headers:inherited="));
    assert_eq!(
        app.store
            .kv_get(Auth::ADMIN_ID_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("583231")
    );
}

#[tokio::test]
async fn another_github_account_is_refused() {
    let app = TestApp::new().await;
    app.github_accepts("good-code", "mallory").await;
    let (state, login_cookie) = app.start_login().await;

    let response = app.callback("good-code", &state, Some(&login_cookie)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(set_cookie(&response, Cookie::SESSION).is_none());
    let html = body(response).await;
    assert!(html.contains("mallory is not allowed"), "{html}");
    assert_eq!(app.session_count().await, 0);
}

#[tokio::test]
async fn a_bad_state_is_refused_before_talking_to_github() {
    let app = TestApp::new().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&app.github)
        .await;
    let (state, login_cookie) = app.start_login().await;

    // A state that does not match the browser's login cookie.
    let response = app.callback("code", "forged", Some(&login_cookie)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body(response).await.contains("expired or was already used"));
    // The right state, but from a browser that did not start the login.
    let response = app.callback("code", &state, None).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // A state hoster never issued, even with a matching cookie.
    let response = app
        .callback(
            "code",
            "unknown",
            Some(&format!("{}=unknown", Cookie::LOGIN)),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // Missing parameters.
    let response = app.get("/auth/github/callback", Some(&login_cookie)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_replayed_callback_or_code_is_refused() {
    let app = TestApp::new().await;
    app.github_accepts("good-code", "octocat").await;
    let (state, login_cookie) = app.start_login().await;
    let first = app.callback("good-code", &state, Some(&login_cookie)).await;
    assert_eq!(first.status(), StatusCode::SEE_OTHER);

    // The same callback URL again: the state was used up.
    let again = app.callback("good-code", &state, Some(&login_cookie)).await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    assert!(set_cookie(&again, Cookie::SESSION).is_none());

    // The same code with a fresh state: GitHub refuses it.
    let (state, login_cookie) = app.start_login().await;
    let replayed = app.callback("good-code", &state, Some(&login_cookie)).await;
    assert_eq!(replayed.status(), StatusCode::BAD_REQUEST);
    assert!(set_cookie(&replayed, Cookie::SESSION).is_none());
    let html = body(replayed).await;
    assert!(html.contains("GitHub refused the login"), "{html}");
    assert_eq!(app.session_count().await, 1);
}

#[tokio::test]
async fn github_being_down_fails_the_login_gracefully() {
    let app = TestApp::new().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(503).set_body_string("<html>unicorn</html>"))
        .mount(&app.github)
        .await;
    let (state, login_cookie) = app.start_login().await;
    let response = app.callback("code", &state, Some(&login_cookie)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let html = body(response).await;
    assert!(html.contains("GitHub could not be reached"), "{html}");
    assert!(
        html.contains(r#"href="/auth/github/login""#),
        "can retry: {html}"
    );
}

#[tokio::test]
async fn a_denied_authorization_is_explained() {
    let app = TestApp::new().await;
    let response = app
        .get("/auth/github/callback?error=access_denied&state=x", None)
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body(response).await.contains("access_denied"));

    // Anything but an OAuth error code is not echoed onto the page.
    let response = app
        .get(
            "/auth/github/callback?error=Call%20us%20at%20555-0100&state=x",
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let html = body(response).await;
    assert!(!html.contains("555-0100"), "{html}");
    assert!(html.contains("unknown error"), "{html}");
}

#[tokio::test]
async fn a_short_idle_timeout_keeps_a_session_in_use_alive() {
    let idle = Duration::from_secs(30);
    let app = TestApp::with_idle_timeout(idle).await;
    // Last seen 20 s ago: still live, and well within the one-minute write throttle.
    let browser = app
        .sign_in_at(Timestamp::now() - Duration::from_secs(20))
        .await;
    assert_eq!(
        app.get("/", Some(&browser.cookie)).await.status(),
        StatusCode::OK
    );
    let now = Timestamp::now();
    let hash = RandomToken::hash(browser.cookie.split_once('=').unwrap().1);
    let record = app
        .store
        .active_session(&hash, now, now - idle)
        .await
        .unwrap()
        .expect("still live");
    assert!(
        record.last_seen_at > now - Duration::from_secs(5),
        "the request was recorded, so the session outlives the idle timeout while in use"
    );
}

#[tokio::test]
async fn admin_pages_need_a_session() {
    let app = TestApp::new().await;
    for uri in ["/", "/no-such-page", "/stacks"] {
        let response = app.get(uri, None).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(header(&response, "location"), "/auth/login");
    }
    let htmx = app
        .send(
            Request::get("/")
                .header("hx-request", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(htmx.status(), StatusCode::OK);
    assert_eq!(header(&htmx, "hx-redirect"), "/auth/login");

    for uri in ["/healthz", "/auth/login"] {
        assert_eq!(app.get(uri, None).await.status(), StatusCode::OK, "{uri}");
    }
    let bogus = format!("{}=not-a-session", Cookie::SESSION);
    assert_eq!(
        app.get("/", Some(&bogus)).await.status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn the_login_page_sends_signed_in_admins_home() {
    let app = TestApp::new().await;
    let html = body(app.get("/auth/login", None).await).await;
    assert!(html.contains("Sign in with GitHub"));
    assert!(
        !html.contains("hx-headers"),
        "no CSRF token without a session"
    );

    let browser = app.sign_in().await;
    let response = app.get("/auth/login", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(header(&response, "location"), "/");
}

#[tokio::test]
async fn expired_sessions_are_not_accepted() {
    let app = TestApp::new().await;
    let browser = app
        .sign_in_at(Timestamp::now() - Duration::from_secs(13 * 3600))
        .await;
    assert_eq!(
        app.get("/", Some(&browser.cookie)).await.status(),
        StatusCode::SEE_OTHER
    );
    // Idle for longer than the idle timeout, well within the absolute limit.
    let browser = app
        .sign_in_at(Timestamp::now() - Duration::from_secs(2 * 3600))
        .await;
    assert_eq!(
        app.get("/", Some(&browser.cookie)).await.status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn state_changing_requests_need_the_origin_and_the_csrf_token() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let attempts = [
        ("no token", vec![("origin", ORIGIN)]),
        (
            "wrong token",
            vec![("origin", ORIGIN), ("x-csrf-token", "0000")],
        ),
        (
            "foreign origin",
            vec![
                ("origin", "https://evil.example"),
                ("x-csrf-token", &browser.csrf),
            ],
        ),
        (
            "cross-site fetch",
            vec![
                ("origin", ORIGIN),
                ("sec-fetch-site", "cross-site"),
                ("x-csrf-token", &browser.csrf),
            ],
        ),
        (
            "no origin information",
            vec![("x-csrf-token", &browser.csrf)],
        ),
    ];
    for (case, headers) in attempts {
        let mut request = post("/auth/logout").header(header::COOKIE, &browser.cookie);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = app.send(request.body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{case}");
    }
    // Still signed in.
    assert_eq!(
        app.get("/", Some(&browser.cookie)).await.status(),
        StatusCode::OK
    );

    let response = app
        .send(
            post("/auth/logout")
                .header(header::COOKIE, &browser.cookie)
                .header("sec-fetch-site", "same-origin")
                .header("x-csrf-token", &browser.csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(header(&response, "location"), "/auth/login");
    let cleared = set_cookie(&response, Cookie::SESSION).unwrap();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    assert_eq!(
        app.get("/", Some(&browser.cookie)).await.status(),
        StatusCode::SEE_OTHER,
        "the session is gone"
    );
}

#[tokio::test]
async fn unauthenticated_cross_origin_posts_are_refused() {
    let app = TestApp::new().await;
    let response = app
        .send(
            post("/auth/logout-everywhere")
                .header("origin", "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A same-origin POST to an admin page without a session is not allowed either.
    let response = app
        .send(
            post("/")
                .header("origin", ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn webhooks_bypass_sessions_and_csrf() {
    let app = TestApp::new().await;
    let response = app
        .send(
            post("/hooks/deploy")
                .header("origin", "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    // Not refused by the guard; the endpoint itself does not exist yet.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn log_out_everywhere_ends_every_session() {
    let app = TestApp::new().await;
    let laptop = app.sign_in().await;
    let phone = app.sign_in().await;
    let response = app
        .send(
            post("/auth/logout-everywhere")
                .header(header::COOKIE, &laptop.cookie)
                .header("origin", ORIGIN)
                .header("hx-request", "true")
                .header("x-csrf-token", &laptop.csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, "hx-redirect"), "/auth/login");
    for browser in [&laptop, &phone] {
        assert_eq!(
            app.get("/", Some(&browser.cookie)).await.status(),
            StatusCode::SEE_OTHER
        );
    }
}

#[test]
fn cookies_are_found_among_others() {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        HeaderValue::from_static("a=1; __Host-hoster_session=abc; b=2"),
    );
    assert_eq!(Cookie::get(&headers, Cookie::SESSION), Some("abc"));
    assert_eq!(Cookie::get(&headers, Cookie::LOGIN), None);
}

#[test]
fn paths_are_classified() {
    assert_eq!(Access::from("/hooks/deploy"), Access::Webhook);
    assert_eq!(Access::from("/healthz"), Access::Public);
    assert_eq!(Access::from("/auth/login"), Access::Public);
    assert_eq!(Access::from("/assets/htmx.min.0123.js"), Access::Public);
    assert_eq!(Access::from("/authx"), Access::Admin);
    assert_eq!(Access::from("/hooksy"), Access::Admin);
    assert_eq!(Access::from("/"), Access::Admin);
}
