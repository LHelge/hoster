//! Spike v5v (`docs/spikes/per-address-connections.md`): can hoster count the open connections
//! of each client address in Pingora 0.9.1, for ADR-0065's per-address cap?
//!
//! Pingora's `ConnectionFilter` is asked on accept and has no close callback. The prototype
//! here attaches a [`Slot`] to every accepted socket as the `tracer` of its L4 stream, whose
//! `Drop` calls [`Tracing::on_disconnected`] however the connection ends. It is attached
//!
//! - on `:443` by a `PreTlsProcess` callback, which Pingora runs on the raw stream before the
//!   TLS handshake (so a stalled or failed handshake holds and frees a slot too), and
//! - on `:80`, which has no such callback, by a `ServerApp` wrapper that downcasts the plain
//!   stream back to the L4 stream the first time it sees it.
//!
//! The filter only checks the count. The tests run a standalone Pingora server with the same
//! listener setup as hoster's (TLS with h2 and http/1.1 ALPN on one port, plain on the other)
//! and a small `ServeHttp` app in place of the proxy: accepting, the handshake, the keep-alive
//! loop and HTTP/2 are Pingora's code either way.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Mutex;

use async_trait::async_trait;
use pingora::apps::ServerApp;
use pingora::apps::http_app::ServeHttp;
use pingora::listeners::tls::TlsSettings;
use pingora::listeners::{ConnectionFilter, PreTlsProcess};
use pingora::protocols::http::ServerSession;
use pingora::protocols::l4::stream::Stream as L4Stream;
use pingora::protocols::{GetSocketDigest, Stream};
use pingora::server::configuration::ServerConf;
use pingora::server::{RunArgs, Server, ShutdownWatch};
use pingora::services::listening::Service;
use pingora::upstreams::peer::{Tracer, Tracing};

use super::super::{ChannelShutdownWatch, Probe, ProxyEvents};
use super::*;

// ---------------------------------------------------------------------------------------------
// The prototype

/// What the cap counts: an IPv4 address, or an IPv6 /64 (IPv4-mapped addresses are IPv4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key(IpAddr);

impl From<IpAddr> for Key {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => Key(IpAddr::V4(v4)),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Key(IpAddr::V4(v4)),
                None => {
                    let net = u128::from(v6) & !((1u128 << 64) - 1);
                    Key(IpAddr::V6(Ipv6Addr::from(net)))
                }
            },
        }
    }
}

/// The open connections per [`Key`]; a key leaves the map when its count reaches zero.
#[derive(Debug, Clone)]
struct Slots {
    cap: usize,
    open: Arc<Mutex<HashMap<Key, usize>>>,
}

impl Slots {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            open: Arc::default(),
        }
    }

    fn open(&self, ip: IpAddr) -> usize {
        let open = self.open.lock().unwrap();
        open.get(&Key::from(ip)).copied().unwrap_or(0)
    }

    /// The keys in the map; no key stays at zero.
    fn keys(&self) -> usize {
        self.open.lock().unwrap().len()
    }

    /// Takes a slot for `stream`'s peer unless it holds one already.
    fn claim(&self, stream: &mut L4Stream) {
        if stream.tracer.is_some() {
            return;
        }
        let Some(ip) = stream
            .get_socket_digest()
            .and_then(|digest| digest.peer_addr().and_then(|a| a.as_inet()).map(|a| a.ip()))
        else {
            return;
        };
        let key = Key::from(ip);
        *self.open.lock().unwrap().entry(key).or_default() += 1;
        stream.tracer = Some(Tracer(Box::new(Slot {
            slots: Some(self.clone()),
            key,
        })));
    }

    /// [`Slots::claim`] for a stream as Pingora hands it to the app: only a plain L4 stream
    /// can be reached.
    fn claim_plain(&self, stream: Stream) -> Stream {
        if !stream.as_any().is::<L4Stream>() {
            return stream;
        }
        let mut l4 = stream
            .into_any()
            .downcast::<L4Stream>()
            .expect("checked above");
        self.claim(&mut l4);
        l4
    }

    fn release(&self, key: Key) {
        let mut open = self.open.lock().unwrap();
        if let Some(count) = open.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                open.remove(&key);
            }
        }
    }
}

/// One connection's slot, released when Pingora drops its L4 stream.
#[derive(Debug)]
struct Slot {
    /// `None` in a clone, which must not release the slot a second time.
    slots: Option<Slots>,
    key: Key,
}

impl Tracing for Slot {
    fn on_connected(&self) {}

    fn on_disconnected(&self) {
        if let Some(slots) = &self.slots {
            slots.release(self.key);
        }
    }

    fn boxed_clone(&self) -> Box<dyn Tracing> {
        Box::new(Slot {
            slots: None,
            key: self.key,
        })
    }
}

#[async_trait]
impl ConnectionFilter for Slots {
    async fn should_accept(&self, addr: Option<&std::net::SocketAddr>) -> bool {
        addr.is_none_or(|addr| self.open(addr.ip()) < self.cap)
    }
}

#[async_trait]
impl PreTlsProcess for Slots {
    async fn process(&self, stream: &mut L4Stream) -> pingora::Result<()> {
        self.claim(stream);
        Ok(())
    }
}

/// `app` with a slot taken for each plain connection it is handed.
struct Claiming<A> {
    app: Arc<A>,
    slots: Slots,
}

#[async_trait]
impl<A: ServerApp + Send + Sync + 'static> ServerApp for Claiming<A> {
    async fn process_new(
        self: &Arc<Self>,
        stream: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        let stream = self.slots.claim_plain(stream);
        self.app.process_new(stream, shutdown).await
    }

    async fn cleanup(&self) {
        self.app.cleanup().await;
    }
}

// ---------------------------------------------------------------------------------------------
// The server

/// Answers `200 ok`, after `N` ms for `/slow/N`.
struct Answer;

#[async_trait]
impl ServeHttp for Answer {
    async fn response(&self, session: &mut ServerSession) -> http::Response<Vec<u8>> {
        let path = session.req_header().uri.path().to_owned();
        if let Some(ms) = path.strip_prefix("/slow/").and_then(|ms| ms.parse().ok()) {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        http::Response::builder()
            .status(200)
            .header("content-length", "2")
            .body(b"ok".to_vec())
            .unwrap()
    }
}

const HOST: &str = "app.example.com";
/// How much later than expected a slot may be freed on a loaded machine.
const SLACK: Duration = Duration::from_secs(5);
const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// A Pingora server with hoster's listener setup and [`Slots`] on both listeners.
struct Spike {
    slots: Slots,
    tls: SocketAddr,
    plain: SocketAddr,
    ca: TestCa,
    shutdown: watch::Sender<Option<ShutdownSignal>>,
    thread: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
}

impl Spike {
    async fn start(cap: usize) -> Self {
        let slots = Slots::new(cap);
        let ca = TestCa::new();
        let dir = tempfile::tempdir().unwrap();
        let (chain, key) = ca.issue(HOST);
        let (chain_path, key_path) = (dir.path().join("chain.pem"), dir.path().join("key.pem"));
        std::fs::write(&chain_path, chain).unwrap();
        std::fs::write(&key_path, key).unwrap();
        let free = || {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
        };
        let (tls, plain) = (free(), free());

        let conf = ServerConf {
            daemon: false,
            threads: 2,
            grace_period_seconds: Some(1),
            graceful_shutdown_timeout_seconds: Some(1),
            ..ServerConf::default()
        };
        let mut server = Server::new_with_opt_and_conf(None, conf);

        let mut https = Service::new("spike tls".to_owned(), Answer);
        let mut settings =
            TlsSettings::intermediate(chain_path.to_str().unwrap(), key_path.to_str().unwrap())
                .unwrap();
        settings.enable_h2();
        https.add_tls_with_settings(&tls.to_string(), None, settings);
        https.set_connection_filter(Arc::new(slots.clone()));
        https
            .endpoints()
            .set_pre_tls_callback(Arc::new(slots.clone()));
        server.add_service(https);

        let mut http = Service::new(
            "spike plain".to_owned(),
            Claiming {
                app: Arc::new(Answer),
                slots: slots.clone(),
            },
        );
        http.add_tcp(&plain.to_string());
        http.set_connection_filter(Arc::new(slots.clone()));
        server.add_service(http);

        let (shutdown, rx) = watch::channel(None);
        let mut events = ProxyEvents {
            phases: server.watch_execution_phase(),
            probes: vec![Probe::from(tls), Probe::from(plain)],
        };
        let thread = std::thread::spawn(move || {
            server.run(RunArgs {
                shutdown_signal: Box::new(ChannelShutdownWatch::from(rx)),
            })
        });
        events.ready(Duration::from_secs(10)).await.unwrap();
        // The readiness probes held slots for a moment.
        let spike = Spike {
            slots,
            tls,
            plain,
            ca,
            shutdown,
            thread: Some(thread),
            _dir: dir,
        };
        spike.settles(0).await;
        spike
    }

    /// Waits until `127.0.0.1` holds `open` slots, and fails if it does not within [`SLACK`].
    async fn settles(&self, open: usize) {
        let deadline = Instant::now() + SLACK;
        while self.slots.open(LOCALHOST) != open {
            assert!(
                Instant::now() < deadline,
                "{} slots open, expected {open}",
                self.slots.open(LOCALHOST)
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if open == 0 {
            assert_eq!(self.slots.keys(), 0, "a key stayed in the map at zero");
        }
    }

    async fn h1(&self) -> Client<TlsStream<TcpStream>> {
        let tcp = TcpStream::connect(self.tls).await.unwrap();
        let tls = self
            .ca
            .connector(&[b"http/1.1"])
            .connect(ServerName::try_from(HOST).unwrap(), tcp)
            .await
            .unwrap();
        Client(BufReader::new(tls))
    }

    async fn h2(&self) -> H2Client<TlsStream<TcpStream>> {
        let tcp = TcpStream::connect(self.tls).await.unwrap();
        let tls = self
            .ca
            .connector(&[b"h2"])
            .connect(ServerName::try_from(HOST).unwrap(), tcp)
            .await
            .unwrap();
        let mut client = H2Client(tls);
        client
            .0
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .await
            .unwrap();
        client
            .send(&[Frame::new(Frame::SETTINGS, 0, 0, [])])
            .await
            .unwrap();
        client
    }

    async fn plain(&self) -> Client<TcpStream> {
        Client(BufReader::new(
            TcpStream::connect(self.plain).await.unwrap(),
        ))
    }

    /// Stops the server as hoster does and waits for its thread.
    async fn stop(mut self, signal: ShutdownSignal) -> Slots {
        self.shutdown.send_replace(Some(signal));
        let thread = self.thread.take().unwrap();
        tokio::task::spawn_blocking(move || thread.join())
            .await
            .unwrap()
            .expect("the server thread exits");
        self.slots.clone()
    }
}

impl Drop for Spike {
    fn drop(&mut self) {
        self.shutdown
            .send_replace(Some(ShutdownSignal::FastShutdown));
    }
}

const GET: &[u8] = b"GET /x HTTP/1.1\r\nHost: app.example.com\r\n\r\n";

// ---------------------------------------------------------------------------------------------
// The cases

#[test]
fn keys_are_ipv4_addresses_and_ipv6_slash_64s() {
    let key = |ip: &str| Key::from(ip.parse::<IpAddr>().unwrap());
    assert_eq!(key("192.0.2.1"), key("::ffff:192.0.2.1"));
    assert_ne!(key("192.0.2.1"), key("192.0.2.2"));
    assert_eq!(key("2001:db8:1:2::1"), key("2001:db8:1:2:ffff::9"));
    assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
}

/// HTTP/1.1 keep-alive on both listeners: one slot per connection, however many requests.
#[tokio::test]
async fn a_kept_alive_connection_is_counted_once() {
    let spike = Spike::start(256).await;
    let mut tls = spike.h1().await;
    let mut plain = spike.plain().await;
    for _ in 0..3 {
        tls.write(GET).await;
        assert_eq!(tls.reply().await.status, 200);
        plain.write(GET).await;
        assert_eq!(plain.reply().await.status, 200);
        spike.settles(2).await;
    }
    drop(tls);
    spike.settles(1).await;
    drop(plain);
    spike.settles(0).await;
}

/// HTTP/2 with many streams at once: one slot for the connection.
#[tokio::test]
async fn an_h2_connection_with_many_streams_is_counted_once() {
    let spike = Spike::start(256).await;
    let mut client = spike.h2().await;
    let streams: Vec<Frame> = (0..50)
        .map(|i| Frame::get(1 + 2 * i, HOST, "/slow/300"))
        .collect();
    client.send(&streams).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    spike.settles(1).await;
    let mut ended = 0;
    let received = client
        .read_until(SLACK, |frame| {
            frame.ends_stream() && {
                ended += 1;
                ended == 50
            }
        })
        .await;
    assert_eq!(
        received
            .of(Frame::HEADERS)
            .filter(|f| f.status() == Some(200))
            .count(),
        50
    );
    spike.settles(1).await;
    drop(client);
    spike.settles(0).await;
}

/// A client that goes away in the middle of a request header, or while its response is
/// pending, frees its slot.
#[tokio::test]
async fn a_client_that_aborts_mid_request_is_released() {
    let spike = Spike::start(256).await;
    let mut tls = spike.h1().await;
    tls.write(b"GET /x HTTP/1.1\r\nHost: app.example.com\r\n")
        .await;
    let mut plain = spike.plain().await;
    plain.write(b"GET /x HTTP/1.1\r\nHost: app").await;
    let mut pending = spike.h1().await;
    pending
        .write(b"GET /slow/2000 HTTP/1.1\r\nHost: app.example.com\r\n\r\n")
        .await;
    let mut h2 = spike.h2().await;
    h2.send(&[Frame::get(1, HOST, "/slow/2000")]).await.unwrap();
    spike.settles(4).await;
    drop((tls, plain, pending, h2));
    // The pending ones are noticed once their response is written (2 s).
    spike.settles(0).await;
}

/// A TLS handshake holds a slot while it lasts, and frees it when it fails: the slot is taken
/// before the handshake starts.
#[tokio::test]
async fn a_failed_tls_handshake_is_released() {
    let spike = Spike::start(256).await;

    // Half a ClientHello, then nothing.
    let mut stalled = TcpStream::connect(spike.tls).await.unwrap();
    stalled
        .write_all(&[0x16, 0x03, 0x01, 0x02, 0x00, 0x01])
        .await
        .unwrap();
    // Not TLS at all.
    let mut garbage = TcpStream::connect(spike.tls).await.unwrap();
    garbage.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    // A client that does not trust the certificate aborts the handshake.
    let untrusting = TestCa::new().connector(&[b"http/1.1"]);
    let tcp = TcpStream::connect(spike.tls).await.unwrap();
    assert!(
        untrusting
            .connect(ServerName::try_from(HOST).unwrap(), tcp)
            .await
            .is_err()
    );

    spike.settles(1).await;
    drop(stalled);
    spike.settles(0).await;
    drop(garbage);
}

/// Shutting down, gracefully or fast, frees every slot, also of connections that are idle,
/// mid-request or mid-handshake when it starts.
#[tokio::test]
async fn shutdown_releases_every_slot() {
    for signal in [
        ShutdownSignal::GracefulTerminate,
        ShutdownSignal::FastShutdown,
    ] {
        let spike = Spike::start(256).await;
        let mut kept = spike.h1().await;
        kept.write(GET).await;
        assert_eq!(kept.reply().await.status, 200);
        let mut busy = spike.h2().await;
        busy.send(&[Frame::get(1, HOST, "/slow/10000")])
            .await
            .unwrap();
        let _idle_h2 = spike.h2().await;
        let mut plain = spike.plain().await;
        plain.write(b"GET /x HTTP/1.1\r\n").await;
        let mut stalled = TcpStream::connect(spike.tls).await.unwrap();
        stalled.write_all(&[0x16, 0x03, 0x01]).await.unwrap();
        spike.settles(5).await;

        let name = format!("{signal:?}");
        let slots = spike.stop(signal).await;
        assert_eq!(slots.open(LOCALHOST), 0, "{name}");
        assert_eq!(slots.keys(), 0, "{name}");
        drop((kept, busy, plain, stalled));
    }
}

/// The filter refuses beyond the cap. The slot is taken after accept (in the connection's own
/// task), so a burst can get past the check before the count catches up; this measures by how
/// much.
#[tokio::test]
async fn a_burst_overshoots_the_cap_by_little() {
    const CAP: usize = 64;
    const BURST: usize = 1000;
    let spike = Spike::start(CAP).await;
    // Raw TCP connections: the kernel completes them all in the backlog before the proxy has
    // accepted any, the worst case for the check.
    let mut held = Vec::with_capacity(BURST);
    for i in 0..BURST {
        let addr = if i % 2 == 0 { spike.plain } else { spike.tls };
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let open = spike.slots.open(LOCALHOST);
    eprintln!("cap {CAP}, burst of {BURST}: {open} slots taken");
    assert!(open >= CAP, "{open} slots taken");
    assert!(open <= CAP + CAP / 4, "{open} slots taken, cap {CAP}");
    drop(held);
    spike.settles(0).await;
}
