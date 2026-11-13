//! The Axum app (ADR-0021): admin UI routes, GitHub OAuth and sessions (ADR-0031), the live
//! dashboard with hoster's in-process statistics (ADR-0045, read through [`Stats`], see
//! `dashboard.rs`), the event log's feeds and Events page (ADR-0055, see `events.rs`), container
//! logs (ADR-0044, see `containers.rs`), the certificates page (ADR-0036, ADR-0051, see
//! `certificates.rs`), the stack pages with their secrets (ADR-0027, ADR-0029, ADR-0049, see
//! `stacks.rs`), Settings: Registries (ADR-0025), the instance name and notifications (ADR-0058)
//! and read-only views of the configuration (ADR-0024, see `settings.rs`), the deploy webhook
//! (ADR-0042, ADR-0047, see `webhook.rs`), and
//! the System page with self-update (ADR-0052, see `system.rs`).
//!
//! The public surface is [`WebApp`]: [`WebApp::router`] builds the routes, [`WebApp::start`]
//! binds the [`Listen`] address and serves until the root cancellation token is cancelled.
//! Every route sits behind [`Auth::guard`] (see `auth/`): sessions, CSRF and the admin check.
//! Pingora reaches the app on a 0600 Unix socket in the state directory (ADR-0031); a loopback
//! TCP listener (`web.dev_listen`) exists for local development only.
//!
//! The stack file JSON Schema is served at [`StackSchema::ADMIN_PATH`] for editors.
//!
//! Every response carries strict security headers ([`SecurityHeaders`]). All scripts, styles and
//! icons are embedded and served from `/assets/` (see `assets.rs`), so the CSP needs no exceptions.

mod assets;
mod auth;
mod certificates;
mod charts;
mod containers;
mod dashboard;
mod events;
mod icons;
mod logs;
mod pages;
mod settings;
mod stacks;
mod system;
mod ui;
mod webhook;

use logs::LogReader;
pub use system::{LocalLogs, LogFacts, LogStoreState};

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use tokio::net::{TcpListener, UnixListener};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::acme::{CertMap, DnsCheck};
use crate::config::Config;
use crate::installation::Installation;
use crate::instance::Instance;
use crate::orchestrator::Orchestrator;
use crate::proxy::Admission;
use crate::registry::UpdateCheckHandle;
use crate::runtime::ContainerRuntime;
use crate::secrets::SecretStore;
use crate::stack::StackSchema;
use crate::stats::Stats;
use crate::store::Store;
use crate::supervisor::ShutdownRequest;
use crate::update::{GithubReleases, SystemdUserService, Updater};

use self::assets::Assets;
use self::auth::Auth;
use self::certificates::{AcmeDirectory, CertificateAccess, CertificateControl, Certificates};
use self::containers::{ContainerSource, Containers};
use self::dashboard::Dashboard;
use self::events::EventsPage;
pub(crate) use self::icons::Icon;
use self::pages::{HostCard, ShellSource};
use self::settings::{
    EffectiveConfig, Notifications, Registries, RegistryControl, Settings, UpdatesSection,
};
use self::stacks::{StackControl, StackFile, Stacks};
use self::system::{LocalHost, System};
pub(crate) use self::ui::Tone;
use self::webhook::{OidcVerifier, Webhook};

pub use self::settings::RegistryAccess;

/// Where the admin app listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// A 0600 Unix socket; Pingora proxies the admin host to it (production).
    Unix(PathBuf),
    /// A loopback TCP address, for local development only.
    Tcp(SocketAddr),
}

impl From<&Config> for Listen {
    fn from(config: &Config) -> Self {
        match config.web.dev_listen {
            Some(addr) => Listen::Tcp(addr),
            None => Listen::Unix(config.state_dir.join(Config::ADMIN_SOCKET_NAME)),
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

    /// One year, for the admin host alone: its parent domain is not hoster's to speak for
    /// (no `includeSubDomains`), and nobody asked to be on a preload list. The admin host is
    /// only ever served over TLS (ADR-0031), so after one visit a browser no longer makes the
    /// plain-HTTP request that `:80` would redirect, which is the hop that can be intercepted.
    /// Browsers ignore the header on the plain-HTTP dev listener.
    const HSTS: &str = "max-age=31536000";

    async fn apply(mut response: Response) -> Response {
        Self::insert(response.headers_mut());
        response
    }

    fn insert(headers: &mut HeaderMap) {
        for (name, value) in [
            (header::CONTENT_SECURITY_POLICY, Self::CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            // Not `no-referrer`: under it browsers send `Origin: null` on same-origin POSTs,
            // which the CSRF origin check (ADR-0031) then refuses. `same-origin` still sends
            // no referrer to other sites.
            (header::REFERRER_POLICY, "same-origin"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::STRICT_TRANSPORT_SECURITY, Self::HSTS),
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
    containers: Containers,
    certificates: Certificates,
    stacks: Stacks,
    registries: Registries,
    settings: Settings,
    system: System,
    webhook: Webhook,
    stats: Stats,
    shell: ShellSource,
    /// The log store the logs views read (ADR-0067).
    logs: LogReader,
}

impl WebApp {
    /// The app for `config`, with sessions in `store`, containers from `runtime`, stacks managed
    /// through `orchestrator`, the served certificates in `certs` with the DNS check `dns`
    /// (shared with the ACME task), registry credentials in `registry`, the GitHub OAuth App's
    /// credentials read from their files or systemd credentials, the dashboard's `stats`, the
    /// `instance` name (ADR-0058), the `secrets` the notification API key is kept in, and
    /// `shutdown`, which the System page requests after a self-update (ADR-0052), and the proxy's
    /// `admission`, whose descriptor use the System page shows (ADR-0065), the log store's
    /// figures `logs`, which it shows too, and the `log_store` the logs views read (ADR-0067).
    /// Fails when the
    /// credentials are missing. Deploy webhook tokens are verified against GitHub's key set for
    /// the configured audience (ADR-0047).
    #[expect(
        clippy::too_many_arguments,
        reason = "the app's collaborators, each wired by the supervisor"
    )]
    pub fn from_config<R: ContainerRuntime>(
        config: &Config,
        store: Store,
        runtime: Arc<R>,
        orchestrator: Arc<Orchestrator<R>>,
        certs: CertMap,
        dns: DnsCheck,
        registry: RegistryAccess,
        stats: Stats,
        instance: Instance,
        secrets: SecretStore,
        shutdown: ShutdownRequest,
        updates: UpdateCheckHandle,
        admission: Admission,
        logs: Arc<dyn LogFacts>,
        log_store: LogStoreState,
    ) -> anyhow::Result<Self> {
        let installation = Installation::from_env()
            .context("HOME is not set, so the binary layout ~/.local/lib/hoster cannot be found")?;
        let updater = Updater::new(
            installation,
            &config.state_dir,
            Arc::new(GithubReleases::new().context("building the GitHub releases client")?),
            Arc::new(store.clone()),
            Arc::new(SystemdUserService::from_env()),
        );
        let verifier = OidcVerifier::github(config.webhook_audience())
            .context("setting up the deploy webhook")?;
        let auth =
            Auth::from_config(config, store.clone()).context("setting up the GitHub login")?;
        let host = LocalHost::new(runtime.clone(), dns.clone(), config.state_dir.clone())
            .with_admission(admission);
        let certificates = CertificateAccess {
            orchestrator: Arc::clone(&orchestrator),
            certs,
            dns,
            backoffs: Arc::new(store.clone()),
            admin_host: config.admin_host.to_string().to_ascii_lowercase(),
            directory: AcmeDirectory::from(config.acme.directory_url.as_str()),
        };
        let registry = registry.recorded(orchestrator.event_log());
        let notifications = Notifications::new(
            store.clone(),
            secrets.clone(),
            Arc::new(secrets),
            orchestrator.event_log(),
            instance.clone(),
            config
                .admin_origin()
                .parse()
                .context("the admin origin is not a URL")?,
        );
        let updates = UpdatesSection::new(store.clone(), updates);
        let mut app = WebApp::new(
            auth,
            runtime,
            Arc::new(certificates),
            Arc::new(orchestrator),
            Arc::new(registry),
            EffectiveConfig::from(config),
            System::new(updater, shutdown, Arc::new(host)).with_logs(logs),
            verifier,
            store,
            stats,
            instance,
            notifications,
        );
        app.settings = app.settings.with_updates(updates);
        app.logs = LogReader::from(log_store);
        Ok(app)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the app's collaborators, each injected for tests"
    )]
    fn new(
        auth: Auth,
        containers: Arc<dyn ContainerSource>,
        certificates: Arc<dyn CertificateControl>,
        stacks: Arc<dyn StackControl>,
        registries: Arc<dyn RegistryControl>,
        config: EffectiveConfig,
        system: System,
        verifier: OidcVerifier,
        store: Store,
        stats: Stats,
        instance: Instance,
        notifications: Notifications,
    ) -> Self {
        WebApp {
            shell: ShellSource::new(
                Arc::clone(&stacks),
                Arc::clone(&certificates),
                instance.clone(),
                stats.resources().clone(),
            ),
            stats,
            system,
            auth: Arc::new(auth),
            containers: Containers::from(containers),
            certificates: Certificates::from(Arc::clone(&certificates)),
            webhook: Webhook::new(verifier, store, Arc::clone(&stacks)),
            stacks: Stacks::from(stacks),
            settings: Settings::new(
                config,
                Arc::clone(&certificates),
                Arc::clone(&registries),
                instance,
                notifications,
            ),
            registries: Registries::from(registries),
            logs: LogReader::default(),
        }
    }

    /// All routes, behind the authentication guard and with the security headers applied.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/", get(Dashboard::show))
            .route(Dashboard::EVENTS, get(Dashboard::events))
            .route(EventsPage::PATH, get(EventsPage::page))
            .route("/stacks", get(Stacks::list))
            .route("/stacks/{name}", get(Stacks::show))
            .route("/stacks/{name}/events", get(Stacks::events))
            .layer(Extension(self.stats.clone()))
            .route("/stacks/{name}/logs", get(Stacks::logs))
            .route("/containers", get(Containers::list))
            .route("/containers/{name}/logs", get(Containers::log_page))
            .route(
                "/containers/{name}/logs/stream",
                get(Containers::log_stream),
            )
            .layer(Extension(self.containers.clone()))
            .layer(Extension(self.logs.clone()))
            .route("/certificates", get(Certificates::page))
            .route("/certificates/dns", post(Certificates::check_dns))
            .route("/certificates/retry", post(Certificates::retry))
            .route("/certificates/dns/all", post(Certificates::check_all_dns))
            .route(
                "/certificates/retry/failing",
                post(Certificates::retry_failing),
            )
            .layer(Extension(self.certificates.clone()))
            .route("/stacks/{name}/deploy", post(Stacks::deploy))
            .route("/stacks/{name}/remove", post(Stacks::remove))
            .route(
                "/stacks/{name}/environment",
                get(Stacks::environment).post(Stacks::save_secret),
            )
            .route(
                "/stacks/{name}/environment/delete",
                post(Stacks::delete_secret),
            )
            // The environment tab's earlier home: the page redirects, its forms still post.
            .route(
                "/stacks/{name}/secrets",
                get(Stacks::secrets_moved).post(Stacks::save_secret),
            )
            .route("/stacks/{name}/secrets/delete", post(Stacks::delete_secret))
            .route(
                "/stacks/{name}/reresolve/preview",
                post(Stacks::reresolve_preview),
            )
            .route("/stacks/{name}/reresolve", post(Stacks::reresolve))
            .route(
                "/stacks/{name}/updates/preview",
                post(Stacks::update_preview),
            )
            .route("/stacks/{name}/updates", post(Stacks::update))
            .route("/stacks/{name}/deployments", get(Stacks::history))
            .route(
                "/stacks/{name}/deployments/{id}",
                get(Stacks::deployment_page),
            )
            .route(
                "/stacks/{name}/deployments/{id}/events",
                get(Stacks::deployment_events),
            )
            .route(
                "/stacks/{name}/deployments/{id}/redeploy",
                post(Stacks::redeploy),
            )
            .route(
                "/stacks/{name}/deployments/{id}/status",
                get(Stacks::deployment_status),
            )
            .route(
                "/stacks/{name}/deployments/{id}/cancel",
                post(Stacks::cancel),
            )
            .route(
                "/apply",
                get(Stacks::apply_form)
                    .post(Stacks::apply)
                    .layer(DefaultBodyLimit::max(StackFile::MAX_BODY)),
            )
            .route(
                "/apply/preview",
                post(Stacks::preview).layer(DefaultBodyLimit::max(StackFile::MAX_BODY)),
            )
            .route(
                "/apply/check",
                post(Stacks::check).layer(DefaultBodyLimit::max(StackFile::MAX_BODY)),
            )
            .layer(Extension(self.stacks.clone()))
            .route(
                "/settings/registries",
                get(Registries::page).post(Registries::save),
            )
            .route("/settings/registries/delete", post(Registries::delete))
            .route("/settings/registries/test", post(Registries::test))
            .layer(Extension(self.registries.clone()))
            .route("/settings/general/name", post(Settings::save_name))
            .route("/settings/general/name/reset", post(Settings::reset_name))
            .route(
                Notifications::URL,
                get(Settings::notifications).post(Notifications::save),
            )
            .route("/settings/notifications/test", post(Notifications::test))
            .route(
                "/settings/notifications/remove",
                post(Notifications::remove),
            )
            .route(
                "/settings/updates",
                get(Settings::updates).post(Settings::save_updates),
            )
            .route("/settings/updates/check", post(Settings::check_updates))
            .route("/settings", get(Settings::index))
            .route("/settings/{part}", get(Settings::section))
            .layer(Extension(self.settings.clone()))
            .route("/system", get(System::page))
            .route("/system/check", post(System::check))
            .route("/system/update", post(System::update))
            .route("/system/version", get(System::version))
            .layer(Extension(self.system.clone()))
            .route("/hooks/deploy", post(Webhook::deploy))
            .layer(Extension(self.webhook.clone()))
            .route("/healthz", get(WebApp::healthz))
            .route(StackSchema::ADMIN_PATH, get(WebApp::stack_schema))
            .route("/assets/{file}", get(Assets::serve))
            .route(HostCard::PATH, get(HostCard::fragment))
            .merge(Auth::routes())
            .layer(Extension(self.shell.clone()))
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

    /// The stack file JSON Schema, for editors pointed at this host instead of the docs site.
    /// Public like the assets: it describes the file format and nothing of this instance.
    async fn stack_schema() -> impl IntoResponse {
        (
            [
                (header::CONTENT_TYPE, "application/schema+json"),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            StackSchema::get().as_ref(),
        )
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
#[cfg(test)]
mod ui_preview;

/// The admin app with a mock GitHub and fake containers and stacks, for tests elsewhere in the
/// crate (the proxy's admin-host tests).
#[cfg(test)]
pub(crate) use self::auth::tests::TestApp;
