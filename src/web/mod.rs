//! The Axum app (ADR-0021): admin UI routes, GitHub OAuth and sessions (ADR-0031), and the deploy
//! webhook handler (ADR-0042, ADR-0047).
//!
//! The public surface is [`WebApp`]: [`WebApp::router`] builds the routes, [`WebApp::start`]
//! binds the [`Listen`] address and serves until the root cancellation token is cancelled.
//! Every route sits behind [`Auth::guard`] (see `auth/`): sessions, CSRF and the admin check.
//! Pingora reaches the app on a 0600 Unix socket in the state directory (ADR-0031); a loopback
//! TCP listener (`web.dev_listen`) exists for local development only.
//!
//! Every response carries strict security headers ([`SecurityHeaders`]). All scripts and styles
//! are embedded and served from `/assets/` (see `assets.rs`), so the CSP needs no exceptions.

mod assets;
mod auth;
mod pages;

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::net::{TcpListener, UnixListener};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::store::Store;

use self::assets::Assets;
use self::auth::Auth;
use self::pages::Dashboard;

/// Where the admin app listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// A 0600 Unix socket; Pingora proxies the admin host to it (production).
    Unix(PathBuf),
    /// A loopback TCP address, for local development only.
    Tcp(SocketAddr),
}

impl Listen {
    /// The socket's file name in the state directory.
    const SOCKET_NAME: &str = "admin.sock";
}

impl From<&Config> for Listen {
    fn from(config: &Config) -> Self {
        match config.web.dev_listen {
            Some(addr) => Listen::Tcp(addr),
            None => Listen::Unix(config.state_dir.join(Listen::SOCKET_NAME)),
        }
    }
}

impl fmt::Display for Listen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Listen::Unix(path) => write!(f, "unix:{}", path.display()),
            Listen::Tcp(addr) => write!(f, "http://{addr}"),
        }
    }
}

/// The security headers set on every response.
struct SecurityHeaders;

impl SecurityHeaders {
    /// Everything comes from hoster itself; no inline scripts or styles, no `eval`, no framing.
    const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                       img-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; \
                       form-action 'self'; frame-ancestors 'none'";

    async fn apply(mut response: Response) -> Response {
        Self::insert(response.headers_mut());
        response
    }

    fn insert(headers: &mut HeaderMap) {
        for (name, value) in [
            (header::CONTENT_SECURITY_POLICY, Self::CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (
                header::HeaderName::from_static("cross-origin-opener-policy"),
                "same-origin",
            ),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
    }
}

/// The admin web app.
#[derive(Clone)]
pub struct WebApp {
    auth: Arc<Auth>,
}

impl WebApp {
    /// The app for `config`, with sessions in `store` and the GitHub OAuth App's credentials
    /// read from their files or systemd credentials. Fails when they are missing.
    pub fn from_config(config: &Config, store: Store) -> anyhow::Result<Self> {
        let auth = Auth::from_config(config, store).context("setting up the GitHub login")?;
        Ok(WebApp::from(auth))
    }

    /// All routes, behind the authentication guard and with the security headers applied.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/", get(Dashboard::show))
            .route("/healthz", get(WebApp::healthz))
            .route("/assets/{file}", get(Assets::serve))
            .merge(Auth::routes())
            .fallback(WebApp::not_found)
            .layer(axum::middleware::from_fn_with_state(
                self.auth.clone(),
                Auth::guard,
            ))
            .with_state(self.auth.clone())
            .layer(axum::middleware::map_response(SecurityHeaders::apply))
    }

    /// Liveness probe; unauthenticated and never cached.
    async fn healthz() -> impl IntoResponse {
        ([(header::CACHE_CONTROL, "no-store")], "ok\n")
    }

    async fn not_found() -> impl IntoResponse {
        (StatusCode::NOT_FOUND, "not found\n")
    }

    /// Binds `listen` and serves [`WebApp::router`] in a background task until `token` is
    /// cancelled, then finishes in-flight requests (see [`WebServer::stop`]).
    pub async fn start(
        self,
        listen: Listen,
        token: CancellationToken,
    ) -> anyhow::Result<WebServer> {
        let router = self.router();
        let shutdown = token.clone().cancelled_owned();
        let (task, socket) = match &listen {
            Listen::Unix(path) => {
                let listener = AdminSocket::bind(path)
                    .await
                    .with_context(|| format!("binding the admin socket {}", path.display()))?;
                let task = tokio::spawn(async move {
                    axum::serve(listener, router)
                        .with_graceful_shutdown(shutdown)
                        .await
                });
                (task, Some(path.clone()))
            }
            Listen::Tcp(addr) => {
                let listener = TcpListener::bind(addr)
                    .await
                    .with_context(|| format!("binding the admin app to {addr}"))?;
                tracing::warn!(%addr, "admin app on loopback TCP: for local development only");
                let task = tokio::spawn(async move {
                    axum::serve(listener, router)
                        .with_graceful_shutdown(shutdown)
                        .await
                });
                (task, None)
            }
        };
        tracing::info!(%listen, "admin app listening");
        Ok(WebServer {
            task: Some(task),
            socket,
            token,
        })
    }
}

impl From<Auth> for WebApp {
    fn from(auth: Auth) -> Self {
        WebApp {
            auth: Arc::new(auth),
        }
    }
}

/// The admin app's 0600 Unix socket (ADR-0031).
struct AdminSocket;

impl AdminSocket {
    /// Binds `path`, replacing a stale socket left by a previous run. Refuses to take over a
    /// socket another process still accepts on, and anything that is not a socket.
    ///
    /// A socket counts as stale only when connecting is refused (nobody listens). Any other
    /// connect error (a full backlog, no permission) may well mean a live server, so it is
    /// reported instead of deleting that server's socket.
    async fn bind(path: &Path) -> io::Result<UnixListener> {
        match tokio::fs::symlink_metadata(path).await {
            Ok(meta) if meta.file_type().is_socket() => {
                match tokio::net::UnixStream::connect(path).await {
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "another process is serving on it (is hoster already running?)",
                        ));
                    }
                    Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {}
                    Err(err) => {
                        return Err(io::Error::new(
                            err.kind(),
                            format!("cannot tell whether the existing socket is stale: {err}"),
                        ));
                    }
                }
                tokio::fs::remove_file(path).await?;
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the path exists and is not a socket",
                ));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let listener = UnixListener::bind(path)?;
        // The state directory is 0700 already; the socket mode is a second fence.
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
        Ok(listener)
    }

    /// Removes the socket file after the server stopped.
    fn remove(path: &Path) {
        if let Err(err) = std::fs::remove_file(path)
            && err.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), error = %err, "could not remove the admin socket");
        }
    }
}

/// A running admin app.
pub struct WebServer {
    /// `None` once the server has stopped.
    task: Option<JoinHandle<io::Result<()>>>,
    /// The Unix socket to remove after stopping.
    socket: Option<PathBuf>,
    token: CancellationToken,
}

impl WebServer {
    /// Stops the server. The serving task stops accepting once the root token is cancelled
    /// (this cancels it too, in case it was not); `graceful` waits for in-flight requests.
    /// Otherwise the serving task is aborted without waiting: connections axum already spawned
    /// are not tracked and end when the runtime shuts down. Cancel-safe: dropping the future
    /// and calling `stop` again keeps waiting for the same task, so a graceful stop can be
    /// escalated.
    pub async fn stop(&mut self, graceful: bool) -> anyhow::Result<()> {
        self.token.cancel();
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        if !graceful {
            task.abort();
        }
        let result = task.await;
        self.task = None;
        if let Some(path) = self.socket.take() {
            AdminSocket::remove(&path);
        }
        match result {
            Ok(served) => served.context("serving the admin app"),
            Err(err) if err.is_cancelled() => Ok(()),
            Err(err) => Err(anyhow::anyhow!("the admin app task panicked: {err}")),
        }
    }
}

#[cfg(test)]
mod tests;
