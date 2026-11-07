//! Spike prototype for Bears task f9t (ADR-0034): can Pingora 0.9 serve on listening sockets
//! handed over by systemd socket activation, so that clients queue in the kernel backlog while
//! hoster restarts instead of being refused?
//!
//! This is **not** production code; the findings are in `docs/spikes/socket-activation.md`.
//!
//! - `serve` runs a Pingora server answering `200 ok`. With `LISTEN_FDS` set (socket activation)
//!   it adopts the inherited listeners; otherwise it binds `--listen` itself, like hoster does
//!   today. `--startup-delay-ms` models hoster's startup work (SQLite, certificates, adopting
//!   containers) before Pingora starts listening.
//! - `probe` opens a fresh connection to an address every `--interval-ms` for `--seconds` and
//!   reports what clients saw: successes, refusals, other errors and the worst latency.
//!
//! Measuring (see the findings note for the exact commands): run `serve` as a transient user
//! service with and without `--socket-property=ListenStream=…`, start `probe`, and
//! `systemctl --user restart` the service while it runs.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{FromRawFd, IntoRawFd, RawFd};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use pingora::apps::http_app::ServeHttp;
use pingora::protocols::http::ServerSession;
use pingora::server::configuration::ServerConf;
use pingora::server::{ListenFds, RunArgs, Server, ShutdownWatch};
use pingora::services::Service;
use pingora::services::listening::Service as ListeningService;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve `200 ok`, on inherited sockets when socket-activated.
    Serve {
        /// Address to bind when not socket-activated.
        #[arg(long, default_value = "127.0.0.1:18080")]
        listen: SocketAddr,
        /// Simulated startup work before Pingora listens.
        #[arg(long, default_value_t = 1000)]
        startup_delay_ms: u64,
    },
    /// Hammer an address with fresh connections and summarise what clients saw.
    Probe {
        addr: SocketAddr,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long, default_value_t = 10)]
        interval_ms: u64,
    },
}

/// A listening socket inherited from systemd, prepared for Pingora.
struct InheritedListener {
    addr: SocketAddr,
    fd: RawFd,
}

impl InheritedListener {
    /// Takes over `fd`. systemd hands sockets over in blocking mode unless the socket unit sets
    /// `NonBlocking=yes`; tokio requires non-blocking ones, and Pingora's fd path does not set it
    /// (its own upgrade path only ever transfers sockets it created non-blocking).
    fn adopt(fd: RawFd) -> io::Result<Self> {
        // SAFETY: `fd` comes from `sd_notify::listen_fds`, which systemd passed to this process
        // and nothing else in it owns.
        let listener = unsafe { TcpListener::from_raw_fd(fd) };
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        Ok(InheritedListener {
            addr,
            fd: listener.into_raw_fd(),
        })
    }

    /// Every socket systemd passed, or none when not socket-activated.
    fn from_env() -> io::Result<Vec<Self>> {
        sd_notify::listen_fds()?.map(Self::adopt).collect()
    }
}

/// Wraps a listening service and seeds Pingora's fd table with the inherited sockets before the
/// inner service builds its listeners. Pingora's `ListenerEndpointBuilder::listen` looks the bind
/// address up in that table and adopts a matching fd instead of binding, exactly as it does for
/// fds received during a graceful upgrade. Only public Pingora API is used.
struct Inherited<S> {
    inner: S,
    listeners: Vec<InheritedListener>,
}

#[async_trait]
impl<S: Service> Service for Inherited<S> {
    async fn start_service(
        &mut self,
        fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
        listeners_per_fd: usize,
    ) {
        // `Server::run` always passes its table on unix.
        let fds = fds.expect("pingora passes a listen fd table on unix");
        {
            let mut table = fds.lock();
            for listener in self.listeners.drain(..) {
                // The key must equal the string given to `add_tcp`.
                table.add(listener.addr.to_string(), listener.fd);
            }
        }
        self.inner
            .start_service(Some(fds), shutdown, listeners_per_fd)
            .await;
    }

    fn name(&self) -> &str {
        self.inner.name()
    }

    fn listen_addresses(&self) -> Option<Vec<String>> {
        self.inner.listen_addresses()
    }
}

struct Ok200;

#[async_trait]
impl ServeHttp for Ok200 {
    async fn response(&self, _session: &mut ServerSession) -> http::Response<Vec<u8>> {
        http::Response::builder()
            .status(http::StatusCode::OK)
            .header(http::header::CONTENT_LENGTH, 3)
            .body(b"ok\n".to_vec())
            .expect("static response is valid")
    }
}

fn serve(listen: SocketAddr, startup_delay: Duration) -> io::Result<()> {
    // Read the inherited fds first: `LISTEN_PID` must still match and nothing may run yet.
    let inherited = InheritedListener::from_env()?;
    std::thread::sleep(startup_delay);

    let conf = ServerConf {
        daemon: false,
        threads: 1,
        grace_period_seconds: Some(0),
        graceful_shutdown_timeout_seconds: Some(1),
        ..ServerConf::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, conf);
    let mut http = ListeningService::new("spike http".to_owned(), Ok200);
    if inherited.is_empty() {
        eprintln!("serve: binding {listen} myself");
        http.add_tcp(&listen.to_string());
        server.add_service(http);
    } else {
        for listener in &inherited {
            eprintln!(
                "serve: adopting inherited fd {} for {}",
                listener.fd, listener.addr
            );
            http.add_tcp(&listener.addr.to_string());
        }
        server.add_service(Inherited {
            inner: http,
            listeners: inherited,
        });
    }
    // Pingora's default signal handling (SIGTERM = graceful terminate) is fine for a prototype.
    server.run(RunArgs::default());
    Ok(())
}

/// What a single client attempt saw.
enum Outcome {
    Ok(Duration),
    Refused,
    Failed(io::Error),
}

impl From<SocketAddr> for Outcome {
    fn from(addr: SocketAddr) -> Self {
        let start = Instant::now();
        let attempt = || -> io::Result<String> {
            let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            stream.write_all(b"GET / HTTP/1.1\r\nHost: spike\r\nConnection: close\r\n\r\n")?;
            let mut response = String::new();
            stream.read_to_string(&mut response)?;
            Ok(response)
        };
        match attempt() {
            Ok(response) if response.starts_with("HTTP/1.1 200") => Outcome::Ok(start.elapsed()),
            Ok(response) => Outcome::Failed(io::Error::other(format!(
                "unexpected response {:?}",
                response.lines().next()
            ))),
            Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => Outcome::Refused,
            Err(e) => Outcome::Failed(e),
        }
    }
}

#[derive(Default)]
struct Summary {
    ok: usize,
    refused: usize,
    failed: usize,
    max_latency: Duration,
    longest_outage: Duration,
}

fn probe(addr: SocketAddr, seconds: u64, interval: Duration) {
    let mut summary = Summary::default();
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut outage_start: Option<Instant> = None;
    while Instant::now() < end {
        let sent = Instant::now();
        match Outcome::from(addr) {
            Outcome::Ok(latency) => {
                summary.ok += 1;
                summary.max_latency = summary.max_latency.max(latency);
                if let Some(start) = outage_start.take() {
                    summary.longest_outage = summary.longest_outage.max(sent - start);
                }
            }
            Outcome::Refused => {
                summary.refused += 1;
                outage_start.get_or_insert(sent);
            }
            Outcome::Failed(e) => {
                eprintln!("probe: {e}");
                summary.failed += 1;
                outage_start.get_or_insert(sent);
            }
        }
        std::thread::sleep(interval);
    }
    if let Some(start) = outage_start {
        summary.longest_outage = summary.longest_outage.max(start.elapsed());
    }
    println!(
        "ok={} refused={} other_errors={} max_ok_latency_ms={} longest_outage_ms={}",
        summary.ok,
        summary.refused,
        summary.failed,
        summary.max_latency.as_millis(),
        summary.longest_outage.as_millis()
    );
}

fn main() -> io::Result<()> {
    match Cli::parse().command {
        Command::Serve {
            listen,
            startup_delay_ms,
        } => serve(listen, Duration::from_millis(startup_delay_ms)),
        Command::Probe {
            addr,
            seconds,
            interval_ms,
        } => {
            probe(addr, seconds, Duration::from_millis(interval_ms));
            Ok(())
        }
    }
}
