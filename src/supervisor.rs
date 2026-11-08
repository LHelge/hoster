//! The supervisor behind `hoster run`: the startup and shutdown order of docs/architecture.md §4,
//! OS signals, the root cancellation token (§3.3 #6) and systemd readiness notification
//! (ADR-0023, `Type=notify`).
//!
//! The public surface is [`Supervisor::new`] and [`Supervisor::run`]. Each startup step is a
//! [`Phase`]; later work plugs into the matching arm of [`Supervisor`]'s [`Startup`] impl and, for
//! shutdown, into `Teardown` (its handles and the [`ShutdownPhase`] arms of `Teardown::leave`),
//! leaving the ordering, signal handling and exit behaviour here.

use std::fmt;
use std::future::Future;
use std::io;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context as _;
use arc_swap::ArcSwap;
use pingora::server::ShutdownSignal;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::acme::{CertMap, ChallengeMap};
use crate::config::{Config, ProxyConfig};
use crate::orchestrator::{Orchestrator, Settings, TagResolver};
use crate::ports::PortAllocator;
use crate::proxy::{Proxy, ProxyEvents, ProxyState, ProxyThread};
use crate::registry::{FileCredentials, RegistryClient};
use crate::routing::{Reserved, RouteTable};
use crate::runtime::{PodmanRuntime, PodmanSocket};
use crate::secrets::SecretStore;
use crate::store::Store;
use crate::web::{Listen, WebApp, WebServer};

/// One step of the startup sequence, in order (architecture §4 steps 1–9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// 1. Configuration parsed and tracing initialized (done before the supervisor exists).
    Config,
    /// 2. Open the SQLite state store and run migrations.
    StateStore,
    /// 3. Load the certificates on disk into `CertMap`.
    Certificates,
    /// 4. Connect to podman, negotiate the API version and ping.
    Podman,
    /// 5. Reconcile managed containers against persisted state.
    Reconcile,
    /// 6. Build and store the initial `RouteTable`.
    RouteTable,
    /// 7. Spawn the Pingora thread and wait until its listeners are bound.
    Proxy,
    /// 8. Start the ACME task, the poller and the Axum app.
    Services,
    /// 9. Tell systemd the service is ready.
    Ready,
}

impl Phase {
    /// Every phase, in the order startup runs them.
    pub const ALL: [Phase; 9] = [
        Phase::Config,
        Phase::StateStore,
        Phase::Certificates,
        Phase::Podman,
        Phase::Reconcile,
        Phase::RouteTable,
        Phase::Proxy,
        Phase::Services,
        Phase::Ready,
    ];

    /// The step number in architecture §4.
    pub fn number(self) -> usize {
        Phase::ALL
            .iter()
            .position(|p| *p == self)
            .expect("every phase is in ALL")
            + 1
    }

    /// Runs every phase in order on `startup`, stopping at the first failure.
    pub async fn run_all<S: Startup>(startup: &mut S) -> Result<(), StartupError> {
        for phase in Phase::ALL {
            tracing::debug!(phase = %phase, step = phase.number(), "startup phase");
            startup
                .enter(phase)
                .await
                .map_err(|source| StartupError { phase, source })?;
        }
        Ok(())
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Phase::Config => "configuration",
            Phase::StateStore => "state store",
            Phase::Certificates => "certificates",
            Phase::Podman => "podman connection",
            Phase::Reconcile => "reconciliation",
            Phase::RouteTable => "route table",
            Phase::Proxy => "proxy",
            Phase::Services => "background services",
            Phase::Ready => "readiness notification",
        })
    }
}

/// Something that can carry out the startup phases.
pub trait Startup {
    /// Carries out one phase. An error aborts startup and makes hoster exit non-zero.
    fn enter(&mut self, phase: Phase) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Startup stopped because a phase failed.
#[derive(Debug, thiserror::Error)]
#[error("startup failed in step {} ({phase})", phase.number())]
pub struct StartupError {
    pub phase: Phase,
    #[source]
    pub source: anyhow::Error,
}

/// How hard to shut down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    /// SIGTERM: let deploys reach a safe point and drain the proxy.
    Graceful,
    /// SIGINT, or a second signal during a graceful shutdown: stop without waiting.
    Fast,
}

impl ShutdownMode {
    /// The mode after receiving `signal` while already in this mode (`None`: not shutting down).
    fn escalate(current: Option<Self>, signal: Received) -> Self {
        match (current, signal) {
            (None, Received::Term) => ShutdownMode::Graceful,
            _ => ShutdownMode::Fast,
        }
    }
}

impl fmt::Display for ShutdownMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ShutdownMode::Graceful => "graceful",
            ShutdownMode::Fast => "fast",
        })
    }
}

/// One step of the shutdown sequence (architecture §4, shutdown steps 1–4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownPhase {
    /// 1. Cancel the poller and the Axum app; stop accepting commands.
    StopIntake,
    /// 2. Let in-flight deploy tasks reach a safe point, then persist.
    SettleDeploys,
    /// 3. Tell Pingora to stop.
    StopProxy,
    /// 4. Join the Pingora thread.
    JoinProxy,
}

impl ShutdownPhase {
    const ALL: [ShutdownPhase; 4] = [
        ShutdownPhase::StopIntake,
        ShutdownPhase::SettleDeploys,
        ShutdownPhase::StopProxy,
        ShutdownPhase::JoinProxy,
    ];
}

impl fmt::Display for ShutdownPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ShutdownPhase::StopIntake => "stop intake",
            ShutdownPhase::SettleDeploys => "settle deploys",
            ShutdownPhase::StopProxy => "stop proxy",
            ShutdownPhase::JoinProxy => "join proxy thread",
        })
    }
}

/// A signal the supervisor reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Received {
    Term,
    Int,
}

impl fmt::Display for Received {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Received::Term => "SIGTERM",
            Received::Int => "SIGINT",
        })
    }
}

/// SIGTERM and SIGINT, installed before startup so neither kills the process outright.
struct Signals {
    term: Signal,
    int: Signal,
}

impl Signals {
    fn install() -> io::Result<Self> {
        Ok(Signals {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
        })
    }
}

/// Where the supervisor's signals come from: the OS in production, a script in tests.
trait SignalSource {
    /// Waits for the next signal.
    fn recv(&mut self) -> impl Future<Output = Received> + Send;
}

impl SignalSource for Signals {
    async fn recv(&mut self) -> Received {
        tokio::select! {
            _ = self.term.recv() => Received::Term,
            _ = self.int.recv() => Received::Int,
        }
    }
}

/// How long startup step 7 waits for the proxy's listeners to accept connections.
const PROXY_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a fast shutdown waits for the Pingora thread. Pingora's grace period is an
/// uninterruptible sleep, so a proxy that was already draining when the shutdown escalated may
/// not stop sooner; the process then exits without it.
const FAST_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

impl From<ShutdownMode> for ShutdownSignal {
    fn from(mode: ShutdownMode) -> Self {
        match mode {
            ShutdownMode::Graceful => ShutdownSignal::GracefulTerminate,
            ShutdownMode::Fast => ShutdownSignal::FastShutdown,
        }
    }
}

/// How hard a pending proxy stop request is; requests only ever get harder.
fn severity(signal: Option<&ShutdownSignal>) -> u8 {
    match signal {
        None => 0,
        Some(ShutdownSignal::GracefulTerminate | ShutdownSignal::GracefulUpgrade) => 1,
        Some(ShutdownSignal::FastShutdown) => 2,
    }
}

/// The supervisor's side of the Pingora thread: its join handle, the `pingora_shutdown` watch
/// channel (architecture §3.3 #3) and the `pingora_phase` events (#4).
struct ProxyHandle {
    /// Set once phase 7 spawns the Pingora thread; taken when it is joined.
    thread: Option<JoinHandle<()>>,
    shutdown_tx: watch::Sender<Option<ShutdownSignal>>,
    /// Pingora's lifecycle, set together with `thread`.
    events: Option<ProxyEvents>,
}

impl Default for ProxyHandle {
    fn default() -> Self {
        ProxyHandle {
            thread: None,
            shutdown_tx: watch::Sender::new(None),
            events: None,
        }
    }
}

impl ProxyHandle {
    /// How often [`ProxyHandle::join`] checks whether the Pingora thread has finished.
    const JOIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

    /// Spawns the Pingora thread and waits until its listeners accept connections.
    async fn start(&mut self, config: &ProxyConfig, state: ProxyState) -> anyhow::Result<()> {
        let ProxyThread { thread, events } =
            Proxy::new(config, state).spawn(self.shutdown_tx.subscribe())?;
        self.thread = Some(thread);
        let events = self.events.insert(events);
        events.ready(PROXY_READY_TIMEOUT).await?;
        tracing::info!(
            http = %config.http_listen,
            https = %config.https_listen,
            "proxy is listening"
        );
        Ok(())
    }

    /// Asks Pingora to stop; a later, harder request replaces an earlier one.
    fn signal(&self, mode: ShutdownMode) {
        let signal = ShutdownSignal::from(mode);
        self.shutdown_tx.send_if_modified(|current| {
            let harder = severity(Some(&signal)) > severity(current.as_ref());
            if harder {
                tracing::info!(?signal, "proxy shutdown requested");
                *current = Some(signal);
            }
            harder
        });
    }

    /// Hardens a stop request that was already sent to `mode`; does nothing if Pingora has not
    /// been asked to stop yet, so the shutdown order is kept.
    fn escalate(&self, mode: ShutdownMode) {
        if self.shutdown_tx.borrow().is_some() {
            self.signal(mode);
        }
    }

    /// Resolves if the Pingora thread terminates on its own (the `pingora_phase` channel,
    /// §3.3 #4). Never resolves while there is no proxy thread.
    async fn terminated(&mut self) {
        match self.events.as_mut() {
            Some(events) => events.terminated().await,
            None => std::future::pending().await,
        }
    }

    /// Waits for the Pingora thread to finish; in fast mode at most [`FAST_JOIN_TIMEOUT`].
    /// Cancel-safe: dropping the future and calling `join` again keeps waiting for the same
    /// thread.
    ///
    /// The wait polls [`JoinHandle::is_finished`] instead of running `JoinHandle::join` on
    /// tokio's blocking pool: dropping the orchestrator runtime waits for every blocking task, so
    /// a join left behind by the fast-mode timeout would hold the process until Pingora's grace
    /// period ran out.
    async fn join(&mut self, mode: ShutdownMode) -> anyhow::Result<()> {
        let Some(thread) = self.thread.as_ref() else {
            return Ok(());
        };
        let finished = async {
            while !thread.is_finished() {
                tokio::time::sleep(Self::JOIN_POLL_INTERVAL).await;
            }
        };
        match mode {
            ShutdownMode::Graceful => finished.await,
            ShutdownMode::Fast => {
                if tokio::time::timeout(FAST_JOIN_TIMEOUT, finished)
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        timeout = ?FAST_JOIN_TIMEOUT,
                        "proxy is still draining; exiting without waiting for it"
                    );
                    return Ok(());
                }
            }
        }
        // The thread has finished, so this join does not block.
        let thread = self.thread.take().expect("checked above");
        thread
            .join()
            .map_err(|_| anyhow::anyhow!("the proxy thread panicked"))
    }
}

/// Readiness and stopping notifications to systemd. Does nothing outside systemd
/// (`NOTIFY_SOCKET` unset).
struct Systemd;

impl Systemd {
    fn notify(state: sd_notify::NotifyState<'_>) {
        if let Err(err) = sd_notify::notify(&[state]) {
            tracing::warn!(error = %err, "could not notify systemd");
        }
    }
}

/// Owns the startup and shutdown order of `hoster run`.
pub struct Supervisor {
    config: Config,
    /// The state store, opened in startup step 2.
    store: Option<Store>,
    /// The secret store (ADR-0029), opened with the state store; its key is generated on the
    /// first start.
    secrets: Option<SecretStore>,
    /// The certificates served by SNI, loaded from disk in startup step 3.
    certs: Option<CertMap>,
    /// The HTTP-01 challenges the `:80` listener answers; the ACME task fills it.
    challenges: ChallengeMap,
    /// The podman connection, made in startup step 4.
    runtime: Option<PodmanRuntime>,
    /// The orchestrator, built and reconciled in startup step 5.
    orchestrator: Option<Arc<Orchestrator<PodmanRuntime>>>,
    /// The route table the proxy reads (architecture §3.2); empty until the orchestrator
    /// publishes one.
    routes: Arc<ArcSwap<RouteTable>>,
    teardown: Teardown,
}

impl Supervisor {
    pub fn new(config: Config) -> Self {
        Supervisor {
            config,
            store: None,
            secrets: None,
            certs: None,
            challenges: ChallengeMap::default(),
            runtime: None,
            orchestrator: None,
            routes: Arc::default(),
            teardown: Teardown::default(),
        }
    }

    /// Starts hoster, runs until SIGTERM/SIGINT and shuts down in order. `Ok` means a clean exit;
    /// an error means startup failed or the proxy died.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut signals = Signals::install().context("installing signal handlers")?;

        let started = tokio::select! {
            result = Phase::run_all(&mut self) => Some(result),
            signal = signals.recv() => {
                tracing::info!(%signal, "signal received during startup, aborting startup");
                None
            }
        };
        let mode = match started {
            Some(Err(err)) => {
                tracing::error!(error = %format!("{err:#}"), "startup failed");
                self.teardown.run(ShutdownMode::Fast, &mut signals).await;
                return Err(err.into());
            }
            // Interrupted during startup: nothing is serving yet, so stop fast.
            None => ShutdownMode::Fast,
            Some(Ok(())) => {
                tracing::info!("hoster is ready");
                tokio::select! {
                    signal = signals.recv() => {
                        let mode = ShutdownMode::escalate(None, signal);
                        tracing::info!(%signal, %mode, "shutting down");
                        mode
                    }
                    () = self.teardown.proxy.terminated() => {
                        self.teardown.run(ShutdownMode::Fast, &mut signals).await;
                        anyhow::bail!("the proxy terminated unexpectedly");
                    }
                }
            }
        };
        self.teardown.run(mode, &mut signals).await;
        tracing::info!("hoster stopped");
        Ok(())
    }
}

/// Everything the shutdown sequence stops: the root cancellation token, the Axum app and the
/// proxy. Later tasks add the handles of the poller and the deploy tasks here.
#[derive(Default)]
struct Teardown {
    /// Root cancellation token; every orchestrator task gets a child of it (§3.3 #6).
    token: CancellationToken,
    /// The admin app, started in phase 8.
    web: Option<WebServer>,
    proxy: ProxyHandle,
}

impl Teardown {
    /// Runs the shutdown sequence in `mode`. A signal arriving during a graceful shutdown
    /// escalates to a fast one: a proxy that was already asked to drain is told to stop now, and
    /// the interrupted phase restarts in fast mode.
    async fn run(&mut self, mut mode: ShutdownMode, signals: &mut impl SignalSource) {
        Systemd::notify(sd_notify::NotifyState::Stopping);
        self.token.cancel();
        for phase in ShutdownPhase::ALL {
            tracing::info!(%phase, %mode, "shutdown phase");
            loop {
                tokio::select! {
                    result = self.leave(phase, mode) => {
                        if let Err(err) = result {
                            tracing::error!(%phase, error = %format!("{err:#}"), "shutdown phase failed");
                        }
                        break;
                    }
                    signal = signals.recv(), if mode == ShutdownMode::Graceful => {
                        mode = ShutdownMode::escalate(Some(mode), signal);
                        tracing::warn!(%signal, %mode, "escalating shutdown");
                        self.proxy.escalate(mode);
                    }
                }
            }
        }
    }

    /// Carries out one shutdown phase in `mode`. Must be cancel-safe: an escalating signal drops
    /// it and runs it again in fast mode.
    async fn leave(&mut self, phase: ShutdownPhase, mode: ShutdownMode) -> anyhow::Result<()> {
        match phase {
            // Cancelling the root token (done before the first phase) stops the poller and the
            // Axum app; the orchestrator refuses new commands once it is cancelled.
            // A graceful stop lets in-flight admin requests finish.
            ShutdownPhase::StopIntake => match self.web.as_mut() {
                Some(web) => web.stop(mode == ShutdownMode::Graceful).await,
                None => Ok(()),
            },
            // Deploy tasks observe the token and stop at their next safe point (§5.4); in fast
            // mode they are not waited for.
            ShutdownPhase::SettleDeploys => Ok(()),
            ShutdownPhase::StopProxy => {
                self.proxy.signal(mode);
                Ok(())
            }
            ShutdownPhase::JoinProxy => self.proxy.join(mode).await,
        }
    }
}

impl Startup for Supervisor {
    async fn enter(&mut self, phase: Phase) -> anyhow::Result<()> {
        match phase {
            Phase::Config => {
                tracing::info!(
                    admin_host = %self.config.admin_host,
                    state_dir = %self.config.state_dir.display(),
                    upstream_ports = %self.config.upstream_ports,
                    "configuration loaded"
                );
                Ok(())
            }
            Phase::StateStore => {
                let store = Store::open(&self.config.state_dir).await?;
                tracing::info!(path = %store.path().display(), "state store ready");
                let secrets = SecretStore::open(
                    store.clone(),
                    &self.config.secret_key_file,
                    &self.config.credentials,
                )
                .await
                .context("cannot open the secret store")?;
                self.secrets = Some(secrets);
                self.store = Some(store);
                Ok(())
            }
            Phase::Podman => {
                // Version negotiated and pinged; a missing socket fails fast, naming the path.
                let socket = PodmanSocket::from_env(self.config.podman_socket.as_deref());
                let runtime = PodmanRuntime::connect(&socket)
                    .await
                    .context("cannot connect to podman")?
                    // Until the secret store exists (ADR-0029): one 0600 file per registry host.
                    .with_credentials(Arc::new(FileCredentials::in_state_dir(
                        &self.config.state_dir,
                    )));
                self.runtime = Some(runtime);
                Ok(())
            }
            Phase::Proxy => {
                let state = ProxyState {
                    routes: Arc::clone(&self.routes),
                    certs: self
                        .certs
                        .clone()
                        .context("certificates load before the proxy starts")?,
                    challenges: self.challenges.clone(),
                };
                self.teardown.proxy.start(&self.config.proxy, state).await
            }
            Phase::Certificates => {
                let (certs, report) = CertMap::open(
                    &self.config.state_dir,
                    Some(self.config.acme.directory_url.clone()),
                )?;
                for host in &report.expired {
                    // The ACME task issues these again once it starts (architecture §4 step 3).
                    tracing::warn!(host, "certificate on disk has expired; not serving it");
                }
                tracing::info!(loaded = report.loaded.len(), "certificates loaded");
                for (host, error) in report.invalid {
                    tracing::warn!(
                        host,
                        error = format!("{:#}", anyhow::Error::from(error)),
                        "ignoring unusable certificate on disk"
                    );
                }
                self.certs = Some(certs);
                Ok(())
            }
            Phase::Reconcile => {
                let store = self
                    .store
                    .clone()
                    .context("the state store opens before reconciliation")?;
                let runtime = self
                    .runtime
                    .clone()
                    .context("podman is connected before reconciliation")?;
                let secrets = self
                    .secrets
                    .clone()
                    .context("the secret store opens before reconciliation")?;
                let resolver = RegistryClient::new(Arc::new(FileCredentials::in_state_dir(
                    &self.config.state_dir,
                )))?;
                let orchestrator = Orchestrator::new(
                    store.clone(),
                    Arc::new(runtime),
                    Arc::new(resolver) as Arc<dyn TagResolver>,
                    PortAllocator::new(store, self.config.upstream_ports),
                    secrets,
                    Settings {
                        reserved: Reserved::from(&self.config),
                        ..Settings::default()
                    },
                );
                // Interrupted deployments resolved, containers adopted, started or removed
                // (§4 step 5, §5.4); then the loop keeps them running (ADR-0028).
                orchestrator
                    .recover()
                    .await
                    .context("startup reconciliation failed")?;
                tokio::spawn(
                    Arc::clone(&orchestrator).run_reconciler(self.teardown.token.child_token()),
                );
                // Live services are probed from now on (ADR-0038).
                tokio::spawn(
                    Arc::clone(&orchestrator).run_health_monitor(self.teardown.token.child_token()),
                );
                self.orchestrator = Some(orchestrator);
                Ok(())
            }
            Phase::RouteTable => {
                tracing::info!(%phase, step = phase.number(), "not implemented yet, skipping");
                Ok(())
            }
            Phase::Services => {
                let listen = Listen::from(&self.config);
                let store = self
                    .store
                    .clone()
                    .context("the state store opens before the admin app")?;
                let runtime = Arc::new(
                    self.runtime
                        .clone()
                        .context("podman is connected before the admin app")?,
                );
                // The one orchestrator, built in step 5: it is the single writer of desired
                // state (ADR-0050), so the web UI must not get a second one.
                let orchestrator = self
                    .orchestrator
                    .clone()
                    .context("the orchestrator is built before the admin app")?;
                let web = WebApp::from_config(&self.config, store, runtime, orchestrator)?
                    .start(listen, self.teardown.token.child_token())
                    .await?;
                self.teardown.web = Some(web);
                Ok(())
            }
            Phase::Ready => {
                Systemd::notify(sd_notify::NotifyState::Ready);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    /// The pending proxy stop request, for comparisons (`ShutdownSignal` is not `PartialEq`).
    fn requested(signal: &Option<ShutdownSignal>) -> String {
        format!("{signal:?}")
    }

    /// Records the phases it was asked to enter and fails at `fail_at`.
    #[derive(Default)]
    struct Recorder {
        entered: Vec<Phase>,
        fail_at: Option<Phase>,
    }

    impl Startup for Recorder {
        async fn enter(&mut self, phase: Phase) -> anyhow::Result<()> {
            self.entered.push(phase);
            if self.fail_at == Some(phase) {
                anyhow::bail!("podman.socket is not running");
            }
            Ok(())
        }
    }

    #[test]
    fn phases_are_numbered_like_the_architecture() {
        let numbers: Vec<_> = Phase::ALL.iter().map(|p| p.number()).collect();
        assert_eq!(numbers, (1..=9).collect::<Vec<_>>());
        assert_eq!(Phase::Proxy.number(), 7);
    }

    #[tokio::test]
    async fn run_all_enters_every_phase_in_order() {
        let mut recorder = Recorder::default();
        Phase::run_all(&mut recorder).await.unwrap();
        assert_eq!(recorder.entered, Phase::ALL);
    }

    #[tokio::test]
    async fn run_all_stops_at_the_first_failure_and_names_it() {
        let mut recorder = Recorder {
            fail_at: Some(Phase::Podman),
            ..Recorder::default()
        };
        let err = Phase::run_all(&mut recorder).await.unwrap_err();
        assert_eq!(recorder.entered.last(), Some(&Phase::Podman));
        assert_eq!(recorder.entered.len(), 4);
        assert_eq!(err.phase, Phase::Podman);
        assert_eq!(
            format!("{:#}", anyhow::Error::from(err)),
            "startup failed in step 4 (podman connection): podman.socket is not running"
        );
    }

    #[test]
    fn signals_escalate_to_fast_shutdown() {
        use ShutdownMode::*;
        assert_eq!(ShutdownMode::escalate(None, Received::Term), Graceful);
        assert_eq!(ShutdownMode::escalate(None, Received::Int), Fast);
        assert_eq!(ShutdownMode::escalate(Some(Graceful), Received::Term), Fast);
        assert_eq!(ShutdownMode::escalate(Some(Graceful), Received::Int), Fast);
    }

    #[test]
    fn proxy_shutdown_never_softens() {
        let proxy = ProxyHandle::default();
        let mut rx = proxy.shutdown_tx.subscribe();
        assert_eq!(requested(&rx.borrow_and_update()), "None");
        proxy.signal(ShutdownMode::Graceful);
        assert_eq!(
            requested(&rx.borrow_and_update()),
            "Some(GracefulTerminate)"
        );
        proxy.signal(ShutdownMode::Fast);
        assert_eq!(requested(&rx.borrow_and_update()), "Some(FastShutdown)");
        proxy.signal(ShutdownMode::Graceful);
        assert!(!rx.has_changed().unwrap());
        assert_eq!(requested(&rx.borrow()), "Some(FastShutdown)");
    }

    #[test]
    fn proxy_escalation_waits_for_the_stop_proxy_phase() {
        let proxy = ProxyHandle::default();
        proxy.escalate(ShutdownMode::Fast);
        assert_eq!(requested(&proxy.shutdown_tx.borrow()), "None");
        proxy.signal(ShutdownMode::Graceful);
        proxy.escalate(ShutdownMode::Fast);
        assert_eq!(requested(&proxy.shutdown_tx.borrow()), "Some(FastShutdown)");
    }

    /// Signals sent by the test.
    struct Scripted(tokio::sync::mpsc::UnboundedReceiver<Received>);

    impl SignalSource for Scripted {
        async fn recv(&mut self) -> Received {
            match self.0.recv().await {
                Some(signal) => signal,
                None => std::future::pending().await,
            }
        }
    }

    /// A stand-in Pingora thread that drains forever on `GracefulTerminate` and only exits on
    /// `FastShutdown`. Returns the teardown and a flag set when the thread exits.
    fn teardown_with_stubborn_proxy() -> (Teardown, Arc<AtomicBool>) {
        let mut teardown = Teardown::default();
        let rx = teardown.proxy.shutdown_tx.subscribe();
        let exited = Arc::new(AtomicBool::new(false));
        let flag = exited.clone();
        teardown.proxy.thread = Some(std::thread::spawn(move || {
            while !matches!(*rx.borrow(), Some(ShutdownSignal::FastShutdown)) {
                std::thread::sleep(Duration::from_millis(5));
            }
            flag.store(true, Ordering::SeqCst);
        }));
        (teardown, exited)
    }

    #[tokio::test]
    async fn graceful_teardown_cancels_and_asks_the_proxy_to_drain() {
        let mut teardown = Teardown::default();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        teardown
            .run(ShutdownMode::Graceful, &mut Scripted(rx))
            .await;
        assert!(teardown.token.is_cancelled());
        assert_eq!(
            requested(&teardown.proxy.shutdown_tx.borrow()),
            "Some(GracefulTerminate)"
        );
    }

    #[tokio::test]
    async fn a_second_signal_while_joining_stops_the_proxy_fast_and_still_joins_it() {
        let (mut teardown, exited) = teardown_with_stubborn_proxy();
        let mut drained = teardown.proxy.shutdown_tx.subscribe();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Send the second SIGTERM once Pingora has been told to drain, i.e. while the
        // supervisor is joining it.
        tokio::spawn(async move {
            drained
                .wait_for(|s| matches!(s, Some(ShutdownSignal::GracefulTerminate)))
                .await
                .unwrap();
            tx.send(Received::Term).unwrap();
            std::future::pending::<()>().await;
        });
        tokio::time::timeout(
            Duration::from_secs(5),
            teardown.run(ShutdownMode::Graceful, &mut Scripted(rx)),
        )
        .await
        .expect("teardown finishes once the proxy is stopped fast");
        assert!(exited.load(Ordering::SeqCst), "the proxy thread was joined");
        assert_eq!(
            requested(&teardown.proxy.shutdown_tx.borrow()),
            "Some(FastShutdown)"
        );
    }

    #[tokio::test]
    async fn joining_without_a_proxy_thread_is_a_no_op() {
        ProxyHandle::default()
            .join(ShutdownMode::Graceful)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn joining_reports_a_panicked_proxy_thread() {
        let mut proxy = ProxyHandle {
            thread: Some(std::thread::spawn(|| panic!("bind failed"))),
            ..ProxyHandle::default()
        };
        let err = proxy.join(ShutdownMode::Graceful).await.unwrap_err();
        assert!(err.to_string().contains("panicked"));
    }
}
