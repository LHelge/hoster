//! Admin authentication (ADR-0021, ADR-0031): GitHub OAuth login restricted to
//! `HOSTER_ADMIN_GITHUB`, server-side sessions in SQLite and CSRF protection.
//!
//! [`Auth::guard`] runs in front of every route:
//!
//! - `/hooks/*` (the deploy webhook) is passed through untouched; it authenticates itself
//!   (ADR-0047) and never uses cookies.
//! - Every request with a state-changing method must come from the admin origin
//!   (`Sec-Fetch-Site: same-origin` and/or a matching `Origin`), and, when it carries a session,
//!   the session's CSRF token in the `X-CSRF-Token` header. HTMX sends that header from the
//!   `hx-headers` on `<body>`.
//! - `/healthz`, `/assets/*` and `/auth/*` are public; everything else needs a session, and a
//!   browser without one is sent to `/auth/login`.
//!
//! Sessions live 12 h at most and end earlier after `web.session_idle_timeout` without a
//! request. The cookie `__Host-hoster_session` holds a random token; SQLite keeps its SHA-256.

mod github;
mod token;

use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{AppendHeaders, IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;

use crate::config::{Config, GithubHandle, WebConfig};
use crate::store::{SessionRecord, Store, StoreError, Timestamp};

use self::github::GithubError;
pub use self::github::{GithubApp, GithubEndpoints, GithubOAuth, GithubUser};
use self::token::RandomToken;
use super::pages::{Flash, FlashKind, Layout, Page};

/// A signed-in admin session, available to handlers as an `Extension<Session>`.
#[derive(Debug, Clone)]
pub struct Session {
    token_hash: String,
    csrf_token: String,
    login: String,
}

impl Session {
    /// The GitHub handle that logged in.
    pub fn login(&self) -> &str {
        &self.login
    }

    /// The token state-changing requests must echo in `X-CSRF-Token`.
    pub fn csrf_token(&self) -> &str {
        &self.csrf_token
    }
}

#[cfg(test)]
impl Session {
    pub fn for_tests(login: &str, csrf_token: &str) -> Self {
        Session {
            token_hash: String::new(),
            csrf_token: csrf_token.to_owned(),
            login: login.to_owned(),
        }
    }
}

impl From<SessionRecord> for Session {
    fn from(record: SessionRecord) -> Self {
        Session {
            token_hash: record.token_hash,
            csrf_token: record.csrf_token,
            login: record.github_login,
        }
    }
}

/// The settings [`Auth`] works with.
#[derive(Debug, Clone)]
pub struct AuthSettings {
    /// The only account allowed in.
    pub admin: GithubHandle,
    /// `https://<admin_host>` (or the development address): the expected `Origin`.
    pub origin: String,
    pub idle_timeout: Duration,
}

impl From<&Config> for AuthSettings {
    fn from(config: &Config) -> Self {
        AuthSettings {
            admin: config.admin_github.clone(),
            origin: config.admin_origin(),
            idle_timeout: config.web.session_idle_timeout,
        }
    }
}

/// Who may use the admin app, and how they prove it.
pub struct Auth {
    settings: AuthSettings,
    store: Store,
    github: GithubOAuth,
}

/// The session cookie (ADR-0031).
struct Cookie;

impl Cookie {
    const SESSION: &str = "__Host-hoster_session";
    /// Binds a started login to the browser that started it; holds the OAuth `state`.
    const LOGIN: &str = "__Host-hoster_login";

    /// The value of cookie `name` in the request, if any.
    fn get<'h>(headers: &'h HeaderMap, name: &str) -> Option<&'h str> {
        headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }

    fn set(name: &str, value: &str, max_age: Duration) -> HeaderValue {
        let cookie = format!(
            "{name}={value}; Path=/; Max-Age={}; Secure; HttpOnly; SameSite=Lax",
            max_age.as_secs()
        );
        HeaderValue::try_from(cookie).expect("cookie values are hex or base64url")
    }

    fn clear(name: &str) -> HeaderValue {
        Self::set(name, "", Duration::ZERO)
    }
}

/// Which requests need what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    /// The deploy webhook: authenticated by OIDC tokens, never by cookies (ADR-0047).
    Webhook,
    /// Reachable without a session.
    Public,
    /// Needs a session.
    Admin,
}

impl From<&str> for Access {
    fn from(path: &str) -> Self {
        if path == "/hooks" || path.starts_with("/hooks/") {
            Access::Webhook
        } else if path == "/healthz" || path.starts_with("/assets/") || path.starts_with("/auth/") {
            Access::Public
        } else {
            Access::Admin
        }
    }
}

/// Why a login failed; rendered as the login page with an explanation.
#[derive(Debug, thiserror::Error)]
enum LoginError {
    #[error("GitHub did not authorize the login ({0}).")]
    Denied(String),
    #[error("This login link expired or was already used. Please sign in again.")]
    BadState,
    #[error("GitHub refused the login ({0}). Please sign in again.")]
    Rejected(String),
    #[error("GitHub could not be reached to complete the login. Try again once GitHub is back.")]
    Unavailable,
    #[error("The GitHub account {0} is not allowed to use this hoster.")]
    NotAdmin(String),
    #[error("The login could not be recorded; see the hoster log.")]
    Store(#[from] StoreError),
}

impl LoginError {
    /// GitHub sent the browser back with `?error=…`. The value comes from the URL, which anyone
    /// can craft, so only an OAuth error code (RFC 6749 §4.1.2.1, e.g. `access_denied`) is shown.
    fn denied(error: String) -> Self {
        let is_code = !error.is_empty()
            && error.len() <= 64
            && error.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        LoginError::Denied(if is_code {
            error
        } else {
            "unknown error".into()
        })
    }

    fn status(&self) -> StatusCode {
        match self {
            LoginError::Denied(_) | LoginError::BadState | LoginError::Rejected(_) => {
                StatusCode::BAD_REQUEST
            }
            LoginError::Unavailable => StatusCode::BAD_GATEWAY,
            LoginError::NotAdmin(_) => StatusCode::FORBIDDEN,
            LoginError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<GithubError> for LoginError {
    fn from(err: GithubError) -> Self {
        match err {
            GithubError::Rejected(reason) => LoginError::Rejected(reason),
            GithubError::Unavailable(_) => LoginError::Unavailable,
        }
    }
}

impl IntoResponse for LoginError {
    fn into_response(self) -> Response {
        if let LoginError::Store(err) = &self {
            tracing::error!(error = %err, "recording the login failed");
        }
        let page = LoginPage {
            layout: Layout::public().with_flash(Flash {
                kind: FlashKind::Error,
                message: self.to_string(),
            }),
        };
        (
            self.status(),
            AppendHeaders([(header::SET_COOKIE, Cookie::clear(Cookie::LOGIN))]),
            Page(page),
        )
            .into_response()
    }
}

/// The `/auth/login` page.
#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    layout: Layout,
}

/// The callback's query string.
#[derive(Debug, Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

impl Auth {
    /// How long a started login may take.
    const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);
    /// `last_seen_at` is written at most this often per session (and at least twice per idle
    /// timeout, so a short timeout does not end a session that is in use).
    const TOUCH_INTERVAL: Duration = Duration::from_secs(60);
    /// At most this many started logins wait for their callback; older ones are dropped.
    const MAX_PENDING_LOGINS: u32 = 32;
    const CSRF_HEADER: &str = "x-csrf-token";
    const CALLBACK: &str = "/auth/github/callback";
    /// The kv key remembering the admin's numeric GitHub id (ADR-0031 hardening).
    const ADMIN_ID_KEY: &str = "auth.admin_github_id";

    pub fn new(settings: AuthSettings, store: Store, github: GithubOAuth) -> Self {
        Auth {
            settings,
            store,
            github,
        }
    }

    /// The real GitHub client for `config`, reading the OAuth App's credentials.
    pub fn from_config(config: &Config, store: Store) -> anyhow::Result<Self> {
        let read =
            |name: &str, file: Option<&std::path::Path>| -> anyhow::Result<String> {
                let value = config.credentials.read_or_file(name, file)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "the GitHub OAuth App's {name} is not set: provide the systemd credential \
                     {name:?} or set web.{name}_file"
                )
            })?;
                anyhow::ensure!(
                    !value.trim().is_empty(),
                    "the GitHub OAuth App's {name} is empty"
                );
                Ok(value)
            };
        let app = GithubApp {
            client_id: read(
                "github_client_id",
                config.web.github_client_id_file.as_deref(),
            )?,
            client_secret: read(
                "github_client_secret",
                config.web.github_client_secret_file.as_deref(),
            )?,
        };
        let settings = AuthSettings::from(config);
        let redirect = format!("{}{}", settings.origin, Self::CALLBACK);
        let github = GithubOAuth::new(app, &redirect, GithubEndpoints::default())?;
        Ok(Auth::new(settings, store, github))
    }

    /// The `/auth/*` routes.
    pub fn routes() -> Router<Arc<Auth>> {
        Router::new()
            .route("/auth/login", get(Auth::login_page))
            .route("/auth/github/login", get(Auth::start_login))
            .route(Self::CALLBACK, get(Auth::callback))
            .route("/auth/logout", post(Auth::logout))
            .route("/auth/logout-everywhere", post(Auth::logout_everywhere))
    }

    /// The middleware in front of every route; see the module docs.
    pub async fn guard(
        State(auth): State<Arc<Auth>>,
        mut request: Request,
        next: Next,
    ) -> Response {
        let access = Access::from(request.uri().path());
        if access == Access::Webhook {
            return next.run(request).await;
        }
        let changes_state = !matches!(
            *request.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        );
        if changes_state && !auth.same_origin(request.headers()) {
            tracing::warn!(method = %request.method(), path = request.uri().path(), "cross-origin request refused");
            return (StatusCode::FORBIDDEN, "cross-origin request refused\n").into_response();
        }
        let session = match auth.session(request.headers()).await {
            Ok(session) => session,
            Err(err) => {
                tracing::error!(error = %err, "looking up the session failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        if changes_state
            && let Some(session) = &session
            && !request
                .headers()
                .get(Self::CSRF_HEADER)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|sent| RandomToken::eq_secret(sent, &session.csrf_token))
        {
            tracing::warn!(method = %request.method(), path = request.uri().path(), "request without a valid CSRF token refused");
            return (StatusCode::FORBIDDEN, "missing or invalid CSRF token\n").into_response();
        }
        match session {
            Some(session) => {
                request.extensions_mut().insert(session);
            }
            None if access == Access::Admin => {
                return Self::redirect(request.headers(), "/auth/login", !changes_state);
            }
            None => {}
        }
        next.run(request).await
    }

    /// Whether a state-changing request comes from the admin app itself.
    /// `Sec-Fetch-Site`, when sent, must be `same-origin`; `Origin`, when sent, must be the
    /// admin origin; and at least one of them must be there (every current browser sends both
    /// on a POST).
    fn same_origin(&self, headers: &HeaderMap) -> bool {
        let fetch_site = headers.get("sec-fetch-site");
        if fetch_site.is_some_and(|site| site != "same-origin") {
            return false;
        }
        match headers.get(header::ORIGIN) {
            Some(origin) => origin.as_bytes() == self.settings.origin.as_bytes(),
            None => fetch_site.is_some(),
        }
    }

    /// The live session the request's cookie names, refreshing its idle timer.
    async fn session(&self, headers: &HeaderMap) -> Result<Option<Session>, StoreError> {
        let Some(token) = Cookie::get(headers, Cookie::SESSION) else {
            return Ok(None);
        };
        self.active(&RandomToken::hash(token)).await
    }

    /// The live session with `hash`, refreshing its idle timer.
    async fn active(&self, hash: &str) -> Result<Option<Session>, StoreError> {
        let now = Timestamp::now();
        let Some(record) = self
            .store
            .active_session(hash, now, now - self.settings.idle_timeout)
            .await?
        else {
            return Ok(None);
        };
        let touch_every = Self::TOUCH_INTERVAL.min(self.settings.idle_timeout / 2);
        if record.last_seen_at < now - touch_every {
            self.store.touch_session(hash, now).await?;
        }
        Ok(Some(Session::from(record)))
    }

    /// Resolves once `session` has ended (logged out, revoked everywhere, or expired), checking
    /// every `every`, so a long-lived response such as a followed log does not outlive it. A
    /// response that is still open counts as activity, as a request does. A failed check is
    /// returned as an error, and the caller should end the response then too.
    pub async fn session_ended(
        &self,
        session: &Session,
        every: Duration,
    ) -> Result<(), StoreError> {
        let mut checks = tokio::time::interval(every);
        checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; the request was just let through.
        checks.tick().await;
        loop {
            checks.tick().await;
            if self.active(&session.token_hash).await?.is_none() {
                return Ok(());
            }
        }
    }

    /// Sends the browser to `to`: a `303`, or for HTMX requests an `HX-Redirect` (fetch would
    /// follow a 303 silently). `ok` picks 200 (navigation) over 401 (refused action) for the
    /// HTMX case.
    fn redirect(headers: &HeaderMap, to: &'static str, ok: bool) -> Response {
        if headers.contains_key("hx-request") {
            let status = if ok {
                StatusCode::OK
            } else {
                StatusCode::UNAUTHORIZED
            };
            (status, [("hx-redirect", to)]).into_response()
        } else {
            (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
        }
    }

    /// `GET /auth/login`: the sign-in page, or the dashboard when already signed in.
    async fn login_page(session: Option<axum::Extension<Session>>) -> Response {
        if session.is_some() {
            return (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response();
        }
        Page(LoginPage {
            layout: Layout::public(),
        })
        .into_response()
    }

    /// `GET /auth/github/login`: records a fresh `state` + PKCE verifier and sends the browser
    /// to GitHub.
    async fn start_login(State(auth): State<Arc<Auth>>) -> Response {
        let authorization = auth.github.authorize();
        let now = Timestamp::now();
        if let Err(err) = auth
            .store
            .add_oauth_login(
                &authorization.state,
                &authorization.pkce_verifier,
                now,
                now - Self::LOGIN_TTL,
                Self::MAX_PENDING_LOGINS,
            )
            .await
        {
            return LoginError::from(err).into_response();
        }
        (
            StatusCode::SEE_OTHER,
            AppendHeaders([
                (
                    header::LOCATION,
                    HeaderValue::try_from(authorization.url.as_str())
                        .expect("a URL is a valid header value"),
                ),
                (
                    header::SET_COOKIE,
                    Cookie::set(Cookie::LOGIN, &authorization.state, Self::LOGIN_TTL),
                ),
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ]),
        )
            .into_response()
    }

    /// `GET /auth/github/callback`: completes the login.
    async fn callback(
        State(auth): State<Arc<Auth>>,
        headers: HeaderMap,
        Query(callback): Query<Callback>,
    ) -> Result<Response, LoginError> {
        if let Some(error) = callback.error {
            return Err(LoginError::denied(error));
        }
        let (Some(code), Some(state)) = (callback.code, callback.state) else {
            return Err(LoginError::BadState);
        };
        let bound = Cookie::get(&headers, Cookie::LOGIN)
            .is_some_and(|cookie| RandomToken::eq_secret(cookie, &state));
        if !bound {
            return Err(LoginError::BadState);
        }
        let verifier = auth
            .store
            .take_oauth_login(&state, Timestamp::now() - Self::LOGIN_TTL)
            .await?
            .ok_or(LoginError::BadState)?;

        let user = auth
            .github
            .identify(&code, &verifier)
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "GitHub login failed");
                LoginError::from(err)
            })?;
        if !auth.settings.admin.matches(&user.login) {
            tracing::warn!(login = %user.login, github_id = user.id, "login refused: not the admin account");
            return Err(LoginError::NotAdmin(user.login));
        }
        auth.check_admin_id(&user).await;

        let token = RandomToken::generate();
        let now = Timestamp::now();
        let idle_since = now - auth.settings.idle_timeout;
        auth.store.prune_sessions(now, idle_since).await?;
        auth.store
            .create_session(&SessionRecord {
                token_hash: RandomToken::hash(token.as_ref()),
                csrf_token: RandomToken::generate().into(),
                github_login: user.login.clone(),
                github_id: user.id,
                created_at: now,
                last_seen_at: now,
                expires_at: now + WebConfig::SESSION_LIFETIME,
            })
            .await?;
        tracing::info!(login = %user.login, github_id = user.id, "admin logged in");
        Ok((
            StatusCode::SEE_OTHER,
            AppendHeaders([
                (header::LOCATION, HeaderValue::from_static("/")),
                (
                    header::SET_COOKIE,
                    Cookie::set(Cookie::SESSION, token.as_ref(), WebConfig::SESSION_LIFETIME),
                ),
                (header::SET_COOKIE, Cookie::clear(Cookie::LOGIN)),
            ]),
        )
            .into_response())
    }

    /// Warns when the admin handle now belongs to a different GitHub account than last time
    /// (a renamed and re-registered handle, ADR-0031), and remembers the id.
    async fn check_admin_id(&self, user: &GithubUser) {
        let id = user.id.to_string();
        let previous = self.store.kv_get(Self::ADMIN_ID_KEY).await;
        match previous {
            Ok(Some(previous)) if previous != id => tracing::warn!(
                login = %user.login,
                github_id = user.id,
                previous_github_id = %previous,
                "the admin handle belongs to a different GitHub account than at the last login; \
                 if the admin account was renamed, update HOSTER_ADMIN_GITHUB now"
            ),
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "cannot read the last admin GitHub id"),
        }
        if let Err(err) = self.store.kv_set(Self::ADMIN_ID_KEY, &id).await {
            tracing::warn!(error = %err, "cannot record the admin GitHub id");
        }
    }

    /// `POST /auth/logout`: ends this session.
    async fn logout(
        State(auth): State<Arc<Auth>>,
        session: Option<axum::Extension<Session>>,
        headers: HeaderMap,
    ) -> Response {
        if let Some(axum::Extension(session)) = session {
            if let Err(err) = auth.store.remove_session(&session.token_hash).await {
                tracing::error!(error = %err, "ending the session failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            tracing::info!(login = %session.login, "admin logged out");
        }
        Self::logged_out(&headers)
    }

    /// `POST /auth/logout-everywhere`: ends every session, on every device.
    async fn logout_everywhere(
        State(auth): State<Arc<Auth>>,
        session: Option<axum::Extension<Session>>,
        headers: HeaderMap,
    ) -> Response {
        let Some(axum::Extension(session)) = session else {
            return Self::logged_out(&headers);
        };
        match auth.store.remove_all_sessions().await {
            Ok(count) => {
                tracing::info!(login = %session.login, sessions = count, "admin logged out everywhere");
                Self::logged_out(&headers)
            }
            Err(err) => {
                tracing::error!(error = %err, "ending the sessions failed");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }

    fn logged_out(headers: &HeaderMap) -> Response {
        let mut response = Self::redirect(headers, "/auth/login", true);
        response
            .headers_mut()
            .append(header::SET_COOKIE, Cookie::clear(Cookie::SESSION));
        response
    }
}

#[cfg(test)]
pub(super) mod tests;
