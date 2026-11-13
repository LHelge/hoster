//! The embedded Pingora proxy on its own thread (ADR-0008): the `:80` and `:443` services and the
//! rustls `CertResolver` (ADR-0014).
//!
//! [`Proxy::new`] builds a Pingora [`Server`] from hoster's [`ProxyConfig`] (never from Pingora's
//! own CLI or YAML), and [`Proxy::spawn`] runs it on a dedicated `std::thread` with its own tokio
//! runtimes (architecture §3.1). The supervisor controls it only through channels:
//!
//! - the `pingora_shutdown` watch channel (§3.3 #3), read by [`ChannelShutdownWatch`], so Pingora
//!   never installs signal handlers of its own;
//! - the `pingora_phase` broadcast channel (§3.3 #4), wrapped by [`ProxyEvents`], which tells
//!   the supervisor when the listeners accept connections and when Pingora died.
//!
//! `Server::run_forever` (which calls `process::exit`) and daemon mode are never used.
//!
//! Two services share the server (architecture §7):
//!
//! - `:443` ([`ProxyConfig::https_listen`]) is TLS with the SNI [`CertResolver`] and ALPN
//!   h2 + http/1.1 (ADR-0033), serving [`RoutingProxy`]: routing against the published
//!   `ArcSwap<RouteTable>`, the forwarding headers of ADR-0010, per-route timeouts (ADR-0011)
//!   and 421 for a request whose host is not the connection's SNI name (ADR-0032).
//! - `:80` ([`ProxyConfig::http_listen`]) is [`ChallengeOrRedirect`]: ACME HTTP-01 answers from
//!   the [`ChallengeMap`], 308 to HTTPS for known hosts and 404 for the rest. The one thing it
//!   proxies is the OTLP receiver ([`ReceiverRoute`], ADR-0069 §3): `host.containers.internal`
//!   from a peer that is one of the host's own addresses, matched before anything else.
//!
//! Both refuse a new client at accept while hoster has too many descriptors open, or the
//! client's address too many connections ([`Admission`], ADR-0065).

mod admission;
mod plain;
mod receiver;
mod resolver;
mod router;
mod trace;

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::thread::JoinHandle;
use std::time::Duration;

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::apps::HttpServerOptions;
use pingora::listeners::tls::TlsSettings;
use pingora::proxy::{ProxyServiceBuilder, Session, http_proxy};
use pingora::server::configuration::ServerConf;
use pingora::server::{ExecutionPhase, RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use pingora::services::listening::Service;
use tokio::sync::{broadcast, watch};

pub use self::admission::{Admission, FileLimit, FileUse};
use self::plain::ChallengeOrRedirect;
pub use self::receiver::ReceiverRoute;
#[cfg(test)]
pub(crate) use self::receiver::{HostAddresses, Interfaces};
use self::resolver::{CertResolver, SniCapture};
use self::router::RoutingProxy;
use crate::acme::{CertMap, ChallengeMap};
use crate::config::{ProxyConfig, Timeouts};
use crate::logs::LogSink;
use crate::routing::RouteTable;
use crate::stats::Traffic;

/// Why the proxy did not come up.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("cannot listen on {addr}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("cannot spawn the proxy thread")]
    Spawn(#[source] io::Error),
    #[error("the proxy terminated during startup")]
    Terminated,
    #[error("the proxy did not accept connections on {addr} within {timeout:?}")]
    NotReady { addr: SocketAddr, timeout: Duration },
}

/// Pingora's shutdown watcher, fed by the supervisor through the `pingora_shutdown` channel
/// instead of OS signals. `None` means "keep running".
pub struct ChannelShutdownWatch(watch::Receiver<Option<ShutdownSignal>>);

impl From<watch::Receiver<Option<ShutdownSignal>>> for ChannelShutdownWatch {
    fn from(rx: watch::Receiver<Option<ShutdownSignal>>) -> Self {
        ChannelShutdownWatch(rx)
    }
}

#[async_trait]
impl ShutdownSignalWatch for ChannelShutdownWatch {
    async fn recv(&self) -> ShutdownSignal {
        // `recv` takes `&self`, so wait on a clone of the receiver.
        let mut rx = self.0.clone();
        match rx.wait_for(Option::is_some).await {
            Ok(signal) => match signal.as_ref() {
                Some(ShutdownSignal::GracefulTerminate) => ShutdownSignal::GracefulTerminate,
                Some(ShutdownSignal::GracefulUpgrade) => ShutdownSignal::GracefulUpgrade,
                Some(ShutdownSignal::FastShutdown) | None => ShutdownSignal::FastShutdown,
            },
            // The supervisor is gone; nobody will ever ask nicely.
            Err(_) => ShutdownSignal::FastShutdown,
        }
    }
}

/// The shared state the proxy reads on every request or handshake; the orchestrator and the
/// ACME task replace what is inside.
#[derive(Debug, Clone)]
pub struct ProxyState {
    /// The published route table (architecture §6).
    pub routes: Arc<ArcSwap<RouteTable>>,
    /// The certificates the `:443` listener serves, by SNI name.
    pub certs: CertMap,
    /// The HTTP-01 challenges the `:80` listener answers.
    pub challenges: ChallengeMap,
    /// The per-route request statistics the `:443` service counts (ADR-0045).
    pub traffic: Traffic,
    /// Which new client connections both listeners take on (ADR-0065).
    pub admission: Admission,
    /// Where the `:443` service offers the rows of requests routed to a stack (ADR-0067 §5).
    pub logs: LogSink,
    /// Where the `:80` service hands the OTLP receiver's requests from local peers
    /// (ADR-0069 §3).
    pub receiver: ReceiverRoute,
}

/// What hoster allows a client connection on either listener, where Pingora's own default is no
/// bound at all (architecture §9.1). These are fixed, not configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DownstreamLimits {
    /// How long a kept-alive HTTP/1.1 connection may take over each read of its next request
    /// header, in whole seconds (Pingora's unit). Pingora applies the same 60 s to a
    /// connection's first request and then waits forever.
    pub keepalive: Duration,
    /// How long an HTTP/2 connection may stay open with no stream on it.
    pub h2_idle: Duration,
}

impl Default for DownstreamLimits {
    fn default() -> Self {
        Self {
            keepalive: Duration::from_secs(60),
            h2_idle: Duration::from_secs(60),
        }
    }
}

impl DownstreamLimits {
    /// The per-service options: the idle timeout of HTTP/2 connections.
    fn server_options(self) -> HttpServerOptions {
        let mut options = HttpServerOptions::default();
        options.h2_idle_timeout = Some(self.h2_idle);
        options
    }

    /// Bounds the rest of `session` and the wait for the connection's next request: the client
    /// may stall a body read for `timeouts.read` and a response write for `timeouts.write`,
    /// the same patience the route has with its upstream (ADR-0011), so a stream the route
    /// allows is never cut from the client's side first.
    ///
    /// Pingora has no body read timeout for HTTP/2; there the upstream's read timeout ends a
    /// request whose body stalls.
    fn admit(self, session: &mut Session, timeouts: Timeouts) {
        // Keeps `Connection: close` from the client; 0 would mean "no timeout".
        session.set_keepalive(Some(self.keepalive.as_secs().max(1)));
        session.set_read_timeout(Some(timeouts.read));
        session.set_write_timeout(Some(timeouts.write));
    }
}

/// A configured, not yet running Pingora server.
pub struct Proxy {
    server: Server,
    /// Every listen address, checked before and probed after the server starts.
    listen: [SocketAddr; 2],
}

impl Proxy {
    /// Builds the server and its two services from `config`, reading `state` at the time of
    /// each request or handshake.
    pub fn new(config: &ProxyConfig, state: ProxyState) -> Self {
        Self::with_limits(config, state, DownstreamLimits::default())
    }

    /// [`Proxy::new`] with other client limits than the defaults, which are too long to wait
    /// for in a test.
    pub(crate) fn with_limits(
        config: &ProxyConfig,
        state: ProxyState,
        limits: DownstreamLimits,
    ) -> Self {
        let conf = ServerConf {
            daemon: false,
            threads: config.threads.get(),
            grace_period_seconds: Some(config.grace_period.as_secs()),
            graceful_shutdown_timeout_seconds: Some(config.graceful_shutdown_timeout.as_secs()),
            ..ServerConf::default()
        };
        let mut server = Server::new_with_opt_and_conf(None, conf);
        let ProxyState {
            routes,
            certs,
            challenges,
            traffic,
            admission,
            logs,
            receiver,
        } = state;
        let admission = Arc::new(admission);

        let mut https = ProxyServiceBuilder::new(
            &server.configuration,
            RoutingProxy::new(Arc::clone(&routes), config.timeouts, limits, traffic, logs),
        )
        .name("hoster https")
        .server_options(limits.server_options())
        .build();
        https.set_connection_filter(admission.clone());
        let mut tls = TlsSettings::with_callbacks(Box::new(SniCapture))
            .expect("rustls TlsSettings::with_callbacks cannot fail");
        tls.set_cert_resolver(Arc::new(CertResolver::from(certs)));
        tls.enable_h2();
        https.add_tls_with_settings(&config.https_listen.to_string(), None, tls);
        // Each connection's slot under the per-address cap, taken before its TLS handshake.
        https.endpoints().set_pre_tls_callback(admission.clone());
        server.add_service(https);

        // `ProxyServiceBuilder::build` without the builder, so the app can be wrapped: `:80`
        // has no pre-TLS callback, so the app takes each connection's slot.
        let mut app = http_proxy(
            &server.configuration,
            ChallengeOrRedirect::new(routes, challenges, receiver, config.timeouts, limits),
        );
        app.server_options = Some(limits.server_options());
        let mut http = Service::new("hoster http".to_owned(), admission.claiming(app));
        http.set_connection_filter(admission);
        http.add_tcp(&config.http_listen.to_string());
        server.add_service(http);

        Proxy {
            server,
            listen: [config.http_listen, config.https_listen],
        }
    }

    /// Runs the server on a dedicated thread until `shutdown` carries a signal.
    ///
    /// The listen addresses are checked first: Pingora binds inside a service task and only
    /// logs a panic when that fails, so a busy or privileged port would otherwise go unnoticed.
    pub fn spawn(
        self,
        shutdown: impl Into<ChannelShutdownWatch>,
    ) -> Result<ProxyThread, ProxyError> {
        // All are held at once, so two addresses that cannot coexist (`[::]:8080` and
        // `0.0.0.0:8080` on a dual-stack host) fail here too.
        let held = self
            .listen
            .iter()
            .map(|&addr| {
                TcpListener::bind(addr).map_err(|source| ProxyError::Bind { addr, source })
            })
            .collect::<Result<Vec<_>, _>>()?;
        drop(held);
        let phases = self.server.watch_execution_phase();
        let shutdown = shutdown.into();
        let server = self.server;
        let thread = std::thread::Builder::new()
            .name("pingora".to_owned())
            .spawn(move || {
                server.run(RunArgs {
                    shutdown_signal: Box::new(shutdown),
                })
            })
            .map_err(ProxyError::Spawn)?;
        Ok(ProxyThread {
            thread,
            events: ProxyEvents {
                phases,
                probes: self.listen.map(Probe::from).to_vec(),
            },
        })
    }
}

/// The running Pingora thread and its lifecycle events.
pub struct ProxyThread {
    pub thread: JoinHandle<()>,
    pub events: ProxyEvents,
}

/// Where to connect to check that a listener accepts: the listen address, or loopback when it
/// listens on every interface.
#[derive(Debug, Clone, Copy)]
struct Probe(SocketAddr);

impl From<SocketAddr> for Probe {
    fn from(mut addr: SocketAddr) -> Self {
        match addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => addr.set_ip(Ipv4Addr::LOCALHOST.into()),
            IpAddr::V6(ip) if ip.is_unspecified() => addr.set_ip(Ipv6Addr::LOCALHOST.into()),
            _ => {}
        }
        Probe(addr)
    }
}

impl Probe {
    async fn accepts(self) -> bool {
        tokio::net::TcpStream::connect(self.0).await.is_ok()
    }
}

/// Pingora's `ExecutionPhase` broadcast (§3.3 #4), seen from the supervisor.
pub struct ProxyEvents {
    phases: broadcast::Receiver<ExecutionPhase>,
    /// One per listener; all must accept before the proxy is ready.
    probes: Vec<Probe>,
}

impl ProxyEvents {
    /// How often readiness re-checks the listener.
    const PROBE_INTERVAL: Duration = Duration::from_millis(20);

    /// Waits until Pingora reports `Running` and all its listeners accept connections.
    ///
    /// `Running` alone is not enough: Pingora sends it before its service tasks have bound their
    /// listeners. Fails if Pingora terminates first or `timeout` passes; the error names the
    /// first listener that was not accepting yet.
    pub async fn ready(&mut self, timeout: Duration) -> Result<(), ProxyError> {
        let probes = self.probes.clone();
        let mut waiting = probes.first().map(|probe| probe.0);
        let result = tokio::time::timeout(timeout, async {
            self.wait_for(|phase| matches!(phase, ExecutionPhase::Running))
                .await?;
            for probe in &probes {
                waiting = Some(probe.0);
                while !probe.accepts().await {
                    tokio::time::sleep(Self::PROBE_INTERVAL).await;
                }
            }
            Ok(())
        })
        .await;
        result.map_err(|_| ProxyError::NotReady {
            addr: waiting.unwrap_or(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))),
            timeout,
        })?
    }

    /// Resolves once Pingora has terminated or its thread is gone.
    pub async fn terminated(&mut self) {
        let _ = self.wait_for(|_| false).await;
    }

    /// Waits for a phase matching `wanted`; fails on `Terminated` or a closed channel (the
    /// server, which owns the sender, is dropped when its thread exits or panics).
    async fn wait_for(
        &mut self,
        wanted: impl Fn(&ExecutionPhase) -> bool,
    ) -> Result<(), ProxyError> {
        loop {
            match self.phases.recv().await {
                Ok(ExecutionPhase::Terminated) | Err(broadcast::error::RecvError::Closed) => {
                    return Err(ProxyError::Terminated);
                }
                Ok(phase) if wanted(&phase) => return Ok(()),
                Ok(phase) => tracing::debug!(?phase, "proxy phase"),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod thread_tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::num::NonZeroUsize;

    use super::*;

    fn config(http_listen: SocketAddr) -> ProxyConfig {
        // A free port that is not `http_listen` (which the caller may have just freed).
        let https_listen = loop {
            let https = free_port();
            if https != http_listen {
                break https;
            }
        };
        ProxyConfig {
            http_listen,
            https_listen,
            threads: NonZeroUsize::MIN,
            grace_period: Duration::from_secs(1),
            graceful_shutdown_timeout: Duration::from_secs(1),
            ..ProxyConfig::default()
        }
    }

    fn free_port() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    /// Empty shared state; the certificate directory lives as long as the returned guard.
    fn state() -> (ProxyState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let (certs, _) = CertMap::open(dir.path(), None).unwrap();
        let state = ProxyState {
            routes: Arc::default(),
            certs,
            challenges: ChallengeMap::default(),
            traffic: Traffic::default(),
            admission: Admission::default(),
            logs: LogSink::disconnected(),
            receiver: ReceiverRoute::new(dir.path().join("otlp.sock")),
        };
        (state, dir)
    }

    fn get(addr: SocketAddr) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(b"GET /anything HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[tokio::test]
    async fn shutdown_watch_waits_for_a_signal() {
        let (tx, rx) = watch::channel(None);
        let watch = ChannelShutdownWatch::from(rx);
        let pending = tokio::time::timeout(Duration::from_millis(50), watch.recv()).await;
        assert!(pending.is_err(), "no signal yet");
        tx.send_replace(Some(ShutdownSignal::GracefulTerminate));
        assert!(matches!(
            watch.recv().await,
            ShutdownSignal::GracefulTerminate
        ));
        tx.send_replace(Some(ShutdownSignal::FastShutdown));
        assert!(matches!(watch.recv().await, ShutdownSignal::FastShutdown));
    }

    #[tokio::test]
    async fn a_dropped_sender_means_fast_shutdown() {
        let (tx, rx) = watch::channel(None);
        drop(tx);
        let watch = ChannelShutdownWatch::from(rx);
        assert!(matches!(watch.recv().await, ShutdownSignal::FastShutdown));
    }

    #[test]
    fn probes_loopback_for_unspecified_addresses() {
        let probe = Probe::from("0.0.0.0:80".parse::<SocketAddr>().unwrap());
        assert_eq!(probe.0.to_string(), "127.0.0.1:80");
        let probe = Probe::from("[::]:80".parse::<SocketAddr>().unwrap());
        assert_eq!(probe.0.to_string(), "[::1]:80");
        let probe = Probe::from("192.0.2.1:80".parse::<SocketAddr>().unwrap());
        assert_eq!(probe.0.to_string(), "192.0.2.1:80");
    }

    #[tokio::test]
    async fn serves_404_and_stops_on_the_channel() {
        let addr = free_port();
        let (tx, rx) = watch::channel(None);
        let (state, _dir) = state();
        let ProxyThread { thread, mut events } =
            Proxy::new(&config(addr), state).spawn(rx).unwrap();
        events.ready(Duration::from_secs(10)).await.unwrap();

        let response = tokio::task::spawn_blocking(move || get(addr))
            .await
            .unwrap();
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");

        tx.send_replace(Some(ShutdownSignal::FastShutdown));
        tokio::time::timeout(Duration::from_secs(10), events.terminated())
            .await
            .expect("proxy terminates");
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .unwrap()
            .expect("proxy thread exits cleanly");
    }

    fn phase_events(phases: broadcast::Receiver<ExecutionPhase>) -> ProxyEvents {
        ProxyEvents {
            phases,
            probes: vec![Probe::from(free_port())],
        }
    }

    #[tokio::test]
    async fn a_proxy_that_dies_during_startup_is_not_ready() {
        let (tx, rx) = broadcast::channel(8);
        let mut events = phase_events(rx);
        tx.send(ExecutionPhase::Terminated).unwrap();
        let err = events.ready(Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, ProxyError::Terminated), "{err}");

        // A thread that panicked drops the server and with it the sender.
        let (tx, rx) = broadcast::channel::<ExecutionPhase>(8);
        let mut events = phase_events(rx);
        drop(tx);
        let err = events.ready(Duration::from_secs(5)).await.unwrap_err();
        assert!(matches!(err, ProxyError::Terminated), "{err}");
    }

    #[tokio::test]
    async fn running_without_a_listener_times_out() {
        let (tx, rx) = broadcast::channel(8);
        let mut events = phase_events(rx);
        tx.send(ExecutionPhase::Running).unwrap();
        let err = events.ready(Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, ProxyError::NotReady { .. }), "{err}");
        drop(tx);
    }

    #[tokio::test]
    async fn terminated_resolves_when_the_proxy_dies_after_startup() {
        let (tx, rx) = broadcast::channel(8);
        let mut events = phase_events(rx);
        tx.send(ExecutionPhase::GracefulTerminate).unwrap();
        tx.send(ExecutionPhase::Terminated).unwrap();
        tokio::time::timeout(Duration::from_secs(5), events.terminated())
            .await
            .expect("Terminated ends the wait");
    }

    #[tokio::test]
    async fn a_busy_address_fails_before_spawning() {
        let busy = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = busy.local_addr().unwrap();
        let (_tx, rx) = watch::channel(None);
        let (state, _dir) = state();
        let err = Proxy::new(&config(addr), state.clone())
            .spawn(rx)
            .err()
            .unwrap();
        assert!(matches!(err, ProxyError::Bind { .. }), "{err}");
        assert!(err.to_string().contains(&addr.to_string()), "{err}");

        // The TLS listener's address is checked too.
        let (_tx, rx) = watch::channel(None);
        let config = ProxyConfig {
            https_listen: addr,
            http_listen: loop {
                let http = free_port();
                if http != addr {
                    break http;
                }
            },
            ..config(addr)
        };
        let err = Proxy::new(&config, state).spawn(rx).err().unwrap();
        assert!(err.to_string().contains(&addr.to_string()), "{err}");
    }
}
