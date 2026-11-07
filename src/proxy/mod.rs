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
//! For now the only service is a plain-HTTP listener answering 404 to everything; the real
//! `:80` and `:443` services replace it.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::thread::JoinHandle;
use std::time::Duration;

use async_trait::async_trait;
use pingora::apps::http_app::ServeHttp;
use pingora::protocols::http::ServerSession;
use pingora::server::configuration::ServerConf;
use pingora::server::{ExecutionPhase, RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use pingora::services::listening::Service;
use tokio::sync::{broadcast, watch};

use crate::config::ProxyConfig;

#[allow(dead_code, reason = "installed on the :443 listener when it lands")]
mod resolver;

#[allow(
    unused_imports,
    reason = "installed on the :443 listener when it lands"
)]
pub use resolver::CertResolver;

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

/// The temporary plain-HTTP app: 404 for every request until the real proxy exists.
struct NotFound;

#[async_trait]
impl ServeHttp for NotFound {
    async fn response(&self, _session: &mut ServerSession) -> http::Response<Vec<u8>> {
        let body = b"404 Not Found\n".to_vec();
        http::Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .header(http::header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(http::header::CONTENT_LENGTH, body.len())
            .body(body)
            .expect("static response is valid")
    }
}

/// A configured, not yet running Pingora server.
pub struct Proxy {
    server: Server,
    http_listen: SocketAddr,
}

impl Proxy {
    /// Builds the server and its services from `config`.
    pub fn new(config: &ProxyConfig) -> Self {
        let conf = ServerConf {
            daemon: false,
            threads: config.threads.get(),
            grace_period_seconds: Some(config.grace_period.as_secs()),
            graceful_shutdown_timeout_seconds: Some(config.graceful_shutdown_timeout.as_secs()),
            ..ServerConf::default()
        };
        let mut server = Server::new_with_opt_and_conf(None, conf);
        let mut http = Service::new("hoster http".to_owned(), NotFound);
        http.add_tcp(&config.http_listen.to_string());
        server.add_service(http);
        Proxy {
            server,
            http_listen: config.http_listen,
        }
    }

    /// Runs the server on a dedicated thread until `shutdown` carries a signal.
    ///
    /// The listen address is checked first: Pingora binds inside a service task and only logs a
    /// panic when that fails, so a busy or privileged port would otherwise go unnoticed.
    pub fn spawn(
        self,
        shutdown: impl Into<ChannelShutdownWatch>,
    ) -> Result<ProxyThread, ProxyError> {
        let addr = self.http_listen;
        TcpListener::bind(addr).map_err(|source| ProxyError::Bind { addr, source })?;
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
                probe: Probe::from(addr),
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
    probe: Probe,
}

impl ProxyEvents {
    /// How often readiness re-checks the listener.
    const PROBE_INTERVAL: Duration = Duration::from_millis(20);

    /// Waits until Pingora reports `Running` and its listener accepts connections.
    ///
    /// `Running` alone is not enough: Pingora sends it before its service tasks have bound their
    /// listeners. Fails if Pingora terminates first or `timeout` passes.
    pub async fn ready(&mut self, timeout: Duration) -> Result<(), ProxyError> {
        let addr = self.probe.0;
        tokio::time::timeout(timeout, async {
            self.wait_for(|phase| matches!(phase, ExecutionPhase::Running))
                .await?;
            while !self.probe.accepts().await {
                tokio::time::sleep(Self::PROBE_INTERVAL).await;
            }
            Ok(())
        })
        .await
        .map_err(|_| ProxyError::NotReady { addr, timeout })?
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
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::num::NonZeroUsize;

    use super::*;

    fn config(http_listen: SocketAddr) -> ProxyConfig {
        ProxyConfig {
            http_listen,
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
        let ProxyThread { thread, mut events } = Proxy::new(&config(addr)).spawn(rx).unwrap();
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
            probe: Probe::from(free_port()),
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
        let err = Proxy::new(&config(addr)).spawn(rx).err().unwrap();
        assert!(matches!(err, ProxyError::Bind { .. }), "{err}");
        assert!(err.to_string().contains(&addr.to_string()), "{err}");
    }
}
