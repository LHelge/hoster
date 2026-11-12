//! End-to-end tests of the proxy: a real Pingora server on loopback in front of in-process axum
//! upstreams, driven by a small raw HTTP/1.1 client (over rustls for `:443`, with certificates
//! from a test CA) so every byte on the wire is under the test's control.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::Json;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use bytes::Bytes;
use futures_util::StreamExt;
use pingora::server::ShutdownSignal;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::{Admission, DownstreamLimits, Proxy, ProxyError, ProxyState, ProxyThread};
use crate::acme::{CertMap, ChallengeMap};
use crate::config::{ProxyConfig, Timeouts};
use crate::logs::{LogSink, Tap};
use crate::routing::{
    DesiredStack, DesiredState, DrainOutcome, Reserved, RouteTable, Target, UnixUpstream, Upstream,
};
use crate::stack::{
    DEFAULT_DRAIN_TIMEOUT, DEFAULT_STOP_TIMEOUT, Limits, Name, Route, RouteTimeouts, Service,
    Stack, UpdateStrategy,
};
use crate::stats::{Histogram, StatusClass, StatusCounts, Traffic};
use crate::store::RequestRow;
use crate::web::{Listen, TestApp, WebServer};

const CHUNK: usize = 64 * 1024;
static ZEROS: [u8; CHUNK] = [0; CHUNK];

// ---------------------------------------------------------------------------------------------
// The upstream app

#[derive(Clone, Default)]
struct AppState {
    /// Lets the SSE handler send its second event only when the test says so.
    sse_release: Arc<Notify>,
}

fn upstream_app(state: AppState) -> axum::Router {
    axum::Router::new()
        .route("/upload", post(upload))
        .route("/download/{bytes}", get(download))
        .route("/sse", get(sse))
        .route("/slow/{ms}", get(slow))
        .route("/ws", any(ws))
        .fallback(echo)
        .with_state(state)
}

/// The request as the upstream saw it.
async fn echo(req: Request) -> Json<Value> {
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in req.headers() {
        headers
            .entry(name.as_str().to_owned())
            .or_default()
            .push(value.to_str().unwrap_or("<binary>").to_owned());
    }
    Json(json!({
        "target": req.uri().to_string(),
        "path": req.uri().path_and_query().map(|pq| pq.as_str()),
        "headers": headers,
    }))
}

/// Counts the request body without holding it.
async fn upload(req: Request) -> String {
    let mut body = req.into_body().into_data_stream();
    let mut total = 0usize;
    // A body that ends early (a client the proxy gave up on) just ends the count.
    while let Some(Ok(chunk)) = body.next().await {
        total += chunk.len();
    }
    total.to_string()
}

/// `bytes` zero bytes, produced one chunk at a time.
async fn download(Path(bytes): Path<usize>) -> Response {
    let chunks = bytes.div_ceil(CHUNK);
    let stream = futures_util::stream::iter((0..chunks).map(move |i| {
        let len = CHUNK.min(bytes - i * CHUNK);
        Ok::<_, Infallible>(Bytes::from_static(&ZEROS[..len]))
    }));
    Body::from_stream(stream).into_response()
}

/// One event, then a second once the test releases it.
async fn sse(State(state): State<AppState>) -> impl IntoResponse {
    let stream = futures_util::stream::unfold(0, move |n| {
        let release = Arc::clone(&state.sse_release);
        async move {
            match n {
                0 => Some((Ok::<_, Infallible>(Event::default().data("first")), 1)),
                1 => {
                    release.notified().await;
                    Some((Ok(Event::default().data("second")), 2))
                }
                _ => None,
            }
        }
    });
    Sse::new(stream)
}

async fn slow(Path(ms): Path<u64>) -> &'static str {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    "late"
}

async fn ws(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            if let Message::Text(text) = message
                && socket.send(Message::Text(text)).await.is_err()
            {
                break;
            }
        }
    })
}

// ---------------------------------------------------------------------------------------------
// The harness

fn name(s: &str) -> Name {
    s.parse().unwrap()
}

fn route(host: &str, prefix: &str, strip_prefix: bool, timeouts: RouteTimeouts) -> Route {
    Route {
        host: host.parse().unwrap(),
        prefix: prefix.parse().unwrap(),
        strip_prefix,
        port: 8080,
        timeouts,
    }
}

fn service(routes: Vec<Route>) -> Service {
    Service {
        image: "ghcr.io/acme/app".parse().unwrap(),
        depends_on: Default::default(),
        track: None,
        auto_update: Default::default(),
        command: None,
        entrypoint: None,
        env: BTreeMap::new(),
        secrets: BTreeMap::new(),
        volumes: BTreeMap::new(),
        files: BTreeMap::new(),
        health: None,
        update: UpdateStrategy::default(),
        stop_timeout: DEFAULT_STOP_TIMEOUT,
        user: None,
        read_only: false,
        shm_size: None,
        tmpfs: BTreeMap::new(),
        init: false,
        limits: Limits::default(),
        routes,
    }
}

/// A proxy in front of one upstream app, with these routes:
///
/// - `app.example.com` `/` → web; `/api` (stripped) → web; `/keep` → web
/// - `api.example.com` `/v1` → web
/// - `slow.example.com` `/` → web with a 300 ms read timeout
/// - `stream.example.com` `/` → web with one-hour read and write timeouts
/// - `down.example.com` `/` → a port nothing listens on
/// - `reset.example.com` `/` → a listener that drops every connection, counting them
/// - `flaky.example.com` `/` → a keep-alive listener that answers only the first request on
///   each connection and closes on the second, counting requests
/// - `gap.example.com` `/` → a service in its stop-first gap
/// - `raw.example.com` `/`; `/api` (stripped) → the [`Recorder`], which keeps every byte it gets
/// - `admin.example.com` → the app on a Unix socket (reserved host)
/// - `hoster.example.com` → hoster's real admin app on its own 0600 Unix socket (reserved host)
///
/// Every routed host has a certificate from one test CA, and so does `noroute.example.com`,
/// which has no routes.
struct Harness {
    /// The `:443` listener.
    proxy: SocketAddr,
    /// The `:80` listener.
    http: SocketAddr,
    ca: TestCa,
    /// An HTTP/1.1-only client trusting `ca`.
    tls: TlsConnector,
    certs: CertMap,
    challenges: ChallengeMap,
    web: Arc<Upstream>,
    web_port: u16,
    routes: Arc<ArcSwap<RouteTable>>,
    /// The request statistics the proxy counts.
    traffic: Traffic,
    /// The connection cap on both listeners.
    admission: Admission,
    /// The sink the proxy offers request rows to.
    logs: LogSink,
    /// What the proxy offered to `logs`.
    tap: tokio::sync::Mutex<Tap>,
    desired: DesiredState,
    reserved: Reserved,
    state: AppState,
    resets: Arc<AtomicUsize>,
    flaky_requests: Arc<AtomicUsize>,
    /// What the upstream behind `raw.example.com` received.
    recorder: Recorder,
    shutdown: watch::Sender<Option<ShutdownSignal>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// hoster's admin app behind `hoster.example.com`, and the server running it.
    admin: TestApp,
    _admin_server: WebServer,
    _socket_dir: tempfile::TempDir,
}

/// A throwaway CA issuing one leaf per host; the end-to-end deploy test (`crate::e2e_tests`)
/// serves its certificates too.
pub(crate) struct TestCa {
    issuer: Issuer<'static, KeyPair>,
    root: CertificateDer<'static>,
    root_pem: String,
}

impl TestCa {
    pub(crate) fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "hoster test CA");
        let cert = params.self_signed(&key).unwrap();
        TestCa {
            root: cert.der().clone(),
            root_pem: cert.pem(),
            issuer: Issuer::new(params, key),
        }
    }

    /// (fullchain PEM, key PEM) for `host`.
    pub(crate) fn issue(&self, host: &str) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec![host.to_owned()])
            .unwrap()
            .signed_by(&key, &self.issuer)
            .unwrap();
        (
            format!("{}{}", leaf.pem(), self.root_pem),
            key.serialize_pem(),
        )
    }

    /// A ring client trusting only this CA, offering `alpn`.
    fn connector(&self, alpn: &[&[u8]]) -> TlsConnector {
        TlsConnector::from(Arc::new(self.client_config(alpn)))
    }

    /// The rustls client configuration behind [`TestCa::connector`].
    pub(crate) fn client_config(&self, alpn: &[&[u8]]) -> ClientConfig {
        let mut roots = RootCertStore::empty();
        roots.add(self.root.clone()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        config
    }
}

/// The SNI name a client uses for a request `Host`: no port, no trailing dot, lowercase.
fn sni_for(host: &str) -> String {
    RouteTable::normalize_host(host).into_owned()
}

impl Harness {
    async fn start() -> Self {
        Self::start_with(Timeouts::default()).await
    }

    /// The harness with `defaults` as the proxy's timeouts for routes that set none.
    async fn start_with(defaults: Timeouts) -> Self {
        Self::start_limited(defaults, DownstreamLimits::default()).await
    }

    /// The harness with `defaults` as the proxy's timeouts and `limits` for its clients.
    async fn start_limited(defaults: Timeouts, limits: DownstreamLimits) -> Self {
        Self::start_admitting(defaults, limits, Admission::default()).await
    }

    /// The harness with `defaults` and `limits`, refusing clients as `admission` says.
    async fn start_admitting(
        defaults: Timeouts,
        limits: DownstreamLimits,
        admission: Admission,
    ) -> Self {
        Self::start_logging(defaults, limits, admission, 1024).await
    }

    /// The harness with a log sink whose channel holds `log_capacity` request rows.
    async fn start_logging(
        defaults: Timeouts,
        limits: DownstreamLimits,
        admission: Admission,
        log_capacity: usize,
    ) -> Self {
        let state = AppState::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let web_port = listener.local_addr().unwrap().port();
        tokio::spawn(axum::serve(listener, upstream_app(state.clone())).into_future());

        let socket_dir = tempfile::tempdir().unwrap();
        let socket = socket_dir.path().join("admin.sock");
        let unix = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(axum::serve(unix, upstream_app(state.clone())).into_future());

        let admin_dir = socket_dir.path().join("state");
        std::fs::create_dir(&admin_dir).unwrap();
        let admin_socket = admin_dir.join(crate::config::Config::ADMIN_SOCKET_NAME);
        let admin = TestApp::new().await;
        let admin_server = admin
            .app
            .clone()
            .start(
                Listen::Unix(admin_socket.clone()),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();

        let resets = Arc::new(AtomicUsize::new(0));
        let reset_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let reset_port = reset_listener.local_addr().unwrap().port();
        let counter = Arc::clone(&resets);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = reset_listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0; 1024];
                let _ = stream.read(&mut buf).await;
                drop(stream);
            }
        });

        let flaky_requests = Arc::new(AtomicUsize::new(0));
        let flaky_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let flaky_port = flaky_listener.local_addr().unwrap().port();
        let counter = Arc::clone(&flaky_requests);
        tokio::spawn(async move {
            while let Ok((stream, _)) = flaky_listener.accept().await {
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let mut conn = Client(BufReader::new(stream));
                    for answered in 0.. {
                        let Some((_, headers)) = conn.request_head().await else {
                            return;
                        };
                        // A request without a length has no body (RFC 9112 §6.3).
                        if headers.iter().any(|(n, _)| {
                            n.eq_ignore_ascii_case("content-length")
                                || n.eq_ignore_ascii_case("transfer-encoding")
                        }) {
                            conn.body(&headers, |_| {}).await;
                        }
                        counter.fetch_add(1, Ordering::SeqCst);
                        if answered > 0 {
                            return;
                        }
                        conn.write(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                            .await;
                    }
                });
            }
        });

        let recorder = Recorder::default();
        let recorder_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let recorder_port = recorder_listener.local_addr().unwrap().port();
        tokio::spawn(recorder.clone().serve(recorder_listener));

        let down_port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        let none = RouteTimeouts::default;
        let short_read = RouteTimeouts {
            read: Some(Duration::from_millis(300)),
            ..RouteTimeouts::default()
        };
        let long = RouteTimeouts {
            read: Some(Duration::from_secs(3600)),
            write: Some(Duration::from_secs(3600)),
            ..RouteTimeouts::default()
        };
        let services = BTreeMap::from([
            (
                name("web"),
                service(vec![
                    route("app.example.com", "/", false, none()),
                    route("app.example.com", "/api", true, none()),
                    route("app.example.com", "/keep", false, none()),
                    route("api.example.com", "/v1", false, none()),
                    route("slow.example.com", "/", false, short_read),
                    route("stream.example.com", "/", false, long),
                ]),
            ),
            (
                name("down"),
                service(vec![route("down.example.com", "/", false, none())]),
            ),
            (
                name("reset"),
                service(vec![route("reset.example.com", "/", false, none())]),
            ),
            (
                name("flaky"),
                service(vec![route("flaky.example.com", "/", false, none())]),
            ),
            (
                name("gapped"),
                service(vec![route("gap.example.com", "/", false, none())]),
            ),
            (
                name("raw"),
                service(vec![
                    route("raw.example.com", "/", false, none()),
                    route("raw.example.com", "/api", true, none()),
                ]),
            ),
        ]);
        let stack = Stack {
            name: name("site"),
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
            deploy: None,
            services,
        };
        let upstream =
            |port, svc: &str| Arc::new(Upstream::loopback(port, name("site"), name(svc), 1));
        let web = upstream(web_port, "web");
        let desired: DesiredState = [DesiredStack::new(stack)
            .with_live(name("web"), [(8080, Arc::clone(&web))])
            .with_live(name("down"), [(8080, upstream(down_port, "down"))])
            .with_live(name("reset"), [(8080, upstream(reset_port, "reset"))])
            .with_live(name("flaky"), [(8080, upstream(flaky_port, "flaky"))])
            .with_live(name("raw"), [(8080, upstream(recorder_port, "raw"))])
            .with_gap(name("gapped"), Duration::from_secs(7))]
        .into_iter()
        .collect();
        let reserved = Reserved::default()
            .with_unix_host(
                "admin.example.com".parse().unwrap(),
                "admin UI",
                Arc::new(UnixUpstream::from(socket)),
            )
            .with_unix_host(
                "hoster.example.com".parse().unwrap(),
                "admin UI",
                Arc::new(UnixUpstream::from(admin_socket)),
            );
        let table = RouteTable::build(&desired, &reserved).unwrap();

        let ca = TestCa::new();
        let (certs, _) = CertMap::open(socket_dir.path(), None).unwrap();
        for host in [
            "app.example.com",
            "api.example.com",
            "slow.example.com",
            "stream.example.com",
            "down.example.com",
            "reset.example.com",
            "flaky.example.com",
            "gap.example.com",
            "raw.example.com",
            "admin.example.com",
            "hoster.example.com",
            "noroute.example.com",
        ] {
            let (chain, key) = ca.issue(host);
            certs.insert(host, chain, key).unwrap();
        }

        let challenges = ChallengeMap::default();
        let routes = Arc::new(ArcSwap::from_pointee(table));
        let traffic = Traffic::default();
        let (logs, tap) = LogSink::tapped(log_capacity);
        let proxy_state = ProxyState {
            routes: Arc::clone(&routes),
            certs: certs.clone(),
            challenges: challenges.clone(),
            traffic: traffic.clone(),
            admission: admission.clone(),
            logs: logs.clone(),
        };
        let (shutdown, rx) = watch::channel(None);
        // A port free a moment ago can be taken by another test's outgoing connection before
        // the proxy binds it; pick again then.
        let mut attempts = 0;
        let (proxy, http, ProxyThread { thread, mut events }) = loop {
            // Both listeners are held at once, so the two ports differ.
            let bind = || std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let (proxy, http) = {
                let (proxy, http) = (bind(), bind());
                (proxy.local_addr().unwrap(), http.local_addr().unwrap())
            };
            let config = ProxyConfig {
                http_listen: http,
                https_listen: proxy,
                threads: NonZeroUsize::new(2).unwrap(),
                grace_period: Duration::from_secs(1),
                graceful_shutdown_timeout: Duration::from_secs(1),
                timeouts: defaults,
            };
            attempts += 1;
            match Proxy::with_limits(&config, proxy_state.clone(), limits).spawn(rx.clone()) {
                Ok(running) => break (proxy, http, running),
                Err(ProxyError::Bind { .. }) if attempts < 5 => continue,
                Err(e) => panic!("{e}"),
            }
        };
        events.ready(Duration::from_secs(10)).await.unwrap();
        Harness {
            proxy,
            http,
            tls: ca.connector(&[b"http/1.1"]),
            ca,
            certs,
            challenges,
            web,
            web_port,
            routes,
            traffic,
            admission,
            logs,
            tap: tokio::sync::Mutex::new(tap),
            desired,
            reserved,
            state,
            resets,
            flaky_requests,
            recorder,
            shutdown,
            thread: Some(thread),
            admin,
            _admin_server: admin_server,
            _socket_dir: socket_dir,
        }
    }

    /// The next request row the proxy offered to the log store, waiting up to five seconds for
    /// Pingora's `logging` hook, which runs after the response went out.
    async fn next_request(&self) -> RequestRow {
        let mut tap = self.tap.lock().await;
        tokio::time::timeout(Duration::from_secs(5), tap.next_request())
            .await
            .expect("a request row within five seconds")
            .expect("the harness holds a sink")
    }

    /// Publishes a table in which `web` runs on `upstream` instead, as a deploy's swap does.
    fn swap_web(&self, upstream: Arc<Upstream>) {
        self.swap("web", upstream);
    }

    /// Publishes a table in which `service` runs live on `upstream`, as a deploy's swap does.
    fn swap(&self, service: &str, upstream: Arc<Upstream>) {
        let mut desired = self.desired.clone();
        let site = desired.get_mut(&name("site")).unwrap();
        site.services.insert(
            name(service),
            crate::routing::ServiceState::Live {
                upstreams: BTreeMap::from([(8080, upstream)]),
            },
        );
        let table = RouteTable::build(&desired, &self.reserved).unwrap();
        self.routes.store(Arc::new(table));
    }

    /// A TLS handshake with `connector` for the SNI name `sni`.
    async fn handshake(
        &self,
        connector: &TlsConnector,
        sni: &str,
    ) -> std::io::Result<TlsStream<TcpStream>> {
        let tcp = TcpStream::connect(self.proxy).await.unwrap();
        let name = ServerName::try_from(sni.to_owned()).unwrap();
        connector.connect(name, tcp).await
    }

    /// An HTTP/1.1 connection to `:443` for the SNI name `sni`.
    async fn connect(&self, sni: &str) -> Client<TlsStream<TcpStream>> {
        let stream = self
            .handshake(&self.tls, sni)
            .await
            .unwrap_or_else(|e| panic!("handshake for {sni}: {e}"));
        Client(BufReader::new(stream))
    }

    /// A plain connection to `:80`.
    async fn connect_http(&self) -> Client<TcpStream> {
        Client(BufReader::new(TcpStream::connect(self.http).await.unwrap()))
    }

    /// One `GET` with `Connection: close` over TLS for `host`.
    async fn get(&self, host: &str, target: &str) -> Reply {
        self.request(&format!(
            "GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        ))
        .await
    }

    /// Sends `head` over TLS, with the SNI name taken from its `Host` header.
    async fn request(&self, head: &str) -> Reply {
        let host = head
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Host: "))
            .expect("request has a Host header");
        self.request_with_sni(&sni_for(host), head).await
    }

    async fn request_with_sni(&self, sni: &str, head: &str) -> Reply {
        let mut client = self.connect(sni).await;
        client.write(head.as_bytes()).await;
        client.reply().await
    }

    /// One `GET` with `Connection: close` to `:80`.
    async fn get_http(&self, host: &str, target: &str) -> Reply {
        let mut client = self.connect_http().await;
        client
            .write(
                format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        client.reply().await
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown
            .send_replace(Some(ShutdownSignal::FastShutdown));
        if let Some(thread) = self.thread.take() {
            // Don't hold up the test runtime; the thread exits on its own.
            drop(thread);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A minimal HTTP/1.1 client

struct Client<S>(BufReader<S>);

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.text()))
    }
}

/// The upstream's echo of the headers it received.
fn echoed(reply: &Reply, name: &str) -> Vec<String> {
    reply.json()["headers"][name]
        .as_array()
        .map(|values| {
            values
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    /// Writes and flushes `bytes`. Over TLS the flush matters: `write_all` can return while the
    /// last records still sit in the rustls session (when the socket was not writable), and the
    /// server would then wait for them forever.
    async fn write(&mut self, bytes: &[u8]) {
        let stream = self.0.get_mut();
        stream.write_all(bytes).await.unwrap();
        stream.flush().await.unwrap();
    }

    async fn line(&mut self) -> String {
        let mut line = String::new();
        self.0.read_line(&mut line).await.unwrap();
        line.trim_end_matches(['\r', '\n']).to_owned()
    }

    /// A request line and headers as an upstream reads them, or `None` at end of stream.
    async fn request_head(&mut self) -> Option<(String, Vec<(String, String)>)> {
        let request_line = self.line().await;
        if request_line.is_empty() {
            return None;
        }
        Some((request_line, self.headers().await))
    }

    async fn headers(&mut self) -> Vec<(String, String)> {
        let mut headers = Vec::new();
        loop {
            let line = self.line().await;
            if line.is_empty() {
                return headers;
            }
            let (name, value) = line.split_once(':').unwrap();
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }

    /// The status line and headers.
    async fn head(&mut self) -> (u16, Vec<(String, String)>) {
        let status_line = self.line().await;
        let status = status_line
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("bad status line {status_line:?}"));
        (status, self.headers().await)
    }

    /// Feeds the body to `sink` as it arrives: by `Content-Length`, chunked, or until EOF.
    async fn body(&mut self, headers: &[(String, String)], mut sink: impl FnMut(&[u8])) {
        let find = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_ascii_lowercase())
        };
        let mut buf = vec![0; CHUNK];
        if find("transfer-encoding").is_some_and(|te| te.contains("chunked")) {
            loop {
                let size = usize::from_str_radix(self.line().await.split(';').next().unwrap(), 16)
                    .unwrap();
                if size == 0 {
                    while !self.line().await.is_empty() {}
                    return;
                }
                let mut left = size;
                while left > 0 {
                    let n = left.min(buf.len());
                    self.0.read_exact(&mut buf[..n]).await.unwrap();
                    sink(&buf[..n]);
                    left -= n;
                }
                self.line().await;
            }
        } else if let Some(len) = find("content-length") {
            let mut left: usize = len.parse().unwrap();
            while left > 0 {
                let n = left.min(buf.len());
                self.0.read_exact(&mut buf[..n]).await.unwrap();
                sink(&buf[..n]);
                left -= n;
            }
        } else {
            loop {
                let n = self.0.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                sink(&buf[..n]);
            }
        }
    }

    async fn reply(&mut self) -> Reply {
        let (status, headers) = self.head().await;
        let mut body = Vec::new();
        self.body(&headers, |bytes| body.extend_from_slice(bytes))
            .await;
        Reply {
            status,
            headers,
            body,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A recording upstream

/// One request as the [`Recorder`] received it, framed strictly by RFC 9112 §6.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    /// The raw request line and header section, up to and including the empty line.
    head: Vec<u8>,
    /// The decoded body.
    body: Vec<u8>,
}

impl Seen {
    fn line(&self) -> String {
        let end = self.head.windows(2).position(|w| w == b"\r\n");
        String::from_utf8_lossy(&self.head[..end.unwrap_or(0)]).into_owned()
    }

    /// The values of the header `name`, as the bytes between the colon and the line end.
    fn header(&self, name: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.head)
            .split("\r\n")
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
            .collect()
    }

    /// What in the head a less strict server could frame differently than this recorder did
    /// (the disagreement request smuggling needs). Empty for a head hoster may forward.
    fn ambiguities(&self) -> Vec<&'static str> {
        let mut found = Vec::new();
        let text = String::from_utf8_lossy(&self.head);
        let lines: Vec<&str> = text.trim_end_matches("\r\n").split("\r\n").collect();
        if lines.iter().any(|line| line.contains(['\r', '\n', '\0'])) {
            found.push("a bare CR, LF or NUL");
        }
        let fields: Vec<(&str, &str)> = lines[1..]
            .iter()
            .map(|line| line.split_once(':').unwrap_or((line, "")))
            .collect();
        if lines[1..].iter().any(|line| line.starts_with([' ', '\t'])) {
            found.push("a folded header line");
        }
        if fields.iter().any(|(name, _)| {
            name.is_empty() || name.contains(|c: char| c.is_ascii_whitespace() || c.is_control())
        }) {
            found.push("whitespace in a header name");
        }
        let values = |name: &str| -> Vec<&str> {
            fields
                .iter()
                .filter(|(n, _)| n.trim().eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim_matches([' ', '\t']))
                .collect()
        };
        let lengths = values("content-length");
        let encodings = values("transfer-encoding");
        if !lengths.is_empty() && !encodings.is_empty() {
            found.push("Content-Length with Transfer-Encoding");
        }
        if lengths.len() > 1 {
            found.push("more than one Content-Length");
        }
        if lengths
            .iter()
            .any(|v| v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()))
        {
            found.push("a Content-Length that is not a number");
        }
        if encodings.len() > 1 || encodings.iter().any(|v| *v != "chunked") {
            found.push("a Transfer-Encoding other than one `chunked`");
        }
        if values("host").len() > 1 {
            found.push("more than one Host");
        }
        if !lines[0].ends_with(" HTTP/1.1") || lines[0].split(' ').count() != 3 {
            found.push("a request line that is not `method target HTTP/1.1`");
        }
        found
    }

    /// The first request at the start of `buf` and how many bytes it takes, `Ok(None)` while
    /// it is incomplete, `Err` when `buf` does not start with a request.
    fn parse(buf: &[u8]) -> Result<Option<(Seen, usize)>, ()> {
        let Some(head_len) = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|at| at + 4)
        else {
            return Ok(None);
        };
        let head = &buf[..head_len];
        let seen = Seen {
            head: head.to_vec(),
            body: Vec::new(),
        };
        if seen.line().split(' ').count() != 3 {
            return Err(());
        }
        let rest = &buf[head_len..];
        let encodings = seen
            .header("transfer-encoding")
            .join(",")
            .to_ascii_lowercase();
        let lengths = seen.header("content-length");
        let framed = if !encodings.is_empty() {
            // The final coding must be chunked (RFC 9112 §6.3 rule 4).
            if encodings.rsplit(',').next().map(str::trim) != Some("chunked") {
                return Err(());
            }
            Self::dechunk(rest)?
        } else if let Some(first) = lengths.first() {
            if lengths.iter().any(|v| v != first) || !first.bytes().all(|b| b.is_ascii_digit()) {
                return Err(());
            }
            let len: usize = first.parse().map_err(|_| ())?;
            (rest.len() >= len).then(|| (rest[..len].to_vec(), len))
        } else {
            Some((Vec::new(), 0))
        };
        Ok(framed.map(|(body, taken)| {
            let head = head.to_vec();
            (Seen { head, body }, head_len + taken)
        }))
    }

    /// A chunked body at the start of `buf`, decoded, and its encoded length.
    fn dechunk(buf: &[u8]) -> Result<Option<(Vec<u8>, usize)>, ()> {
        let line_end = |from: usize| {
            buf[from..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .map(|at| from + at)
        };
        let (mut at, mut body) = (0, Vec::new());
        loop {
            let Some(end) = line_end(at) else {
                return Ok(None);
            };
            let size = std::str::from_utf8(&buf[at..end]).map_err(|_| ())?;
            let size = size.split(';').next().unwrap_or_default();
            if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(());
            }
            let size = usize::from_str_radix(size, 16).map_err(|_| ())?;
            at = end + 2;
            if size == 0 {
                // Trailer fields up to the empty line.
                loop {
                    let Some(end) = line_end(at) else {
                        return Ok(None);
                    };
                    let empty = end == at;
                    at = end + 2;
                    if empty {
                        return Ok(Some((body, at)));
                    }
                }
            }
            let Some(data) = buf.get(at..at + size + 2) else {
                return Ok(None);
            };
            if &data[size..] != b"\r\n" {
                return Err(());
            }
            body.extend_from_slice(&data[..size]);
            at += size + 2;
        }
    }
}

/// A raw TCP upstream that keeps what it receives, as a strict HTTP/1.1 server frames it, and
/// answers each request `200` with the request line as the body. An HTTP library would
/// normalise or refuse what the adversarial tests need to see.
#[derive(Debug, Clone, Default)]
struct Recorder {
    requests: Arc<std::sync::Mutex<Vec<Seen>>>,
    /// Connections that ended in something other than a whole request: bytes that are not a
    /// request, or a request the proxy gave up on halfway.
    broken: Arc<AtomicUsize>,
}

impl Recorder {
    async fn serve(self, listener: TcpListener) {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(self.clone().connection(stream));
        }
    }

    async fn connection(self, mut stream: TcpStream) {
        let mut buf = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            match Seen::parse(&buf) {
                Ok(Some((seen, taken))) => {
                    let line = seen.line();
                    self.requests.lock().unwrap().push(seen);
                    buf.drain(..taken);
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{line}\n",
                        line.len() + 1
                    );
                    if stream.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
                Ok(None) => match stream.read(&mut chunk).await {
                    Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                    _ if buf.is_empty() => return,
                    _ => break,
                },
                Err(()) => break,
            }
        }
        self.broken.fetch_add(1, Ordering::SeqCst);
    }

    /// The whole requests received since the last call.
    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut self.requests.lock().unwrap())
    }

    /// How many connections have ended without a whole request so far.
    fn broken(&self) -> usize {
        self.broken.load(Ordering::SeqCst)
    }

    /// Waits until one more connection than `before` has ended without a whole request: the
    /// proxy closes an upstream connection it gave up on a moment after answering the client.
    async fn broken_after(&self, before: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.broken() <= before {
            assert!(
                Instant::now() < deadline,
                "no upstream connection was cut short"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A raw HTTP/2 client

/// One HTTP/2 frame (RFC 9113 §4.1).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

impl Frame {
    const DATA: u8 = 0x0;
    const HEADERS: u8 = 0x1;
    const PRIORITY: u8 = 0x2;
    const RST_STREAM: u8 = 0x3;
    const SETTINGS: u8 = 0x4;
    const PING: u8 = 0x6;
    const GOAWAY: u8 = 0x7;
    const CONTINUATION: u8 = 0x9;

    const END_STREAM: u8 = 0x1;
    const END_HEADERS: u8 = 0x4;

    /// Error codes (RFC 9113 §7).
    const PROTOCOL_ERROR: u32 = 0x1;
    const REFUSED_STREAM: u32 = 0x7;
    const CANCEL: u32 = 0x8;
    const ENHANCE_YOUR_CALM: u32 = 0xb;

    fn new(kind: u8, flags: u8, stream: u32, payload: impl Into<Vec<u8>>) -> Self {
        Frame {
            kind,
            flags,
            stream,
            payload: payload.into(),
        }
    }

    /// A HEADERS frame carrying `fields` as one complete header block.
    fn headers(stream: u32, end_stream: bool, fields: &[(&str, &str)]) -> Self {
        let flags = Self::END_HEADERS | if end_stream { Self::END_STREAM } else { 0 };
        Frame::new(Self::HEADERS, flags, stream, Self::block(fields))
    }

    /// A complete bodyless `GET` of `path` on `authority`.
    fn get(stream: u32, authority: &str, path: &str) -> Self {
        Frame::headers(
            stream,
            true,
            &[
                (":method", "GET"),
                (":scheme", "https"),
                (":authority", authority),
                (":path", path),
            ],
        )
    }

    fn data(stream: u32, end_stream: bool, payload: &[u8]) -> Self {
        let flags = if end_stream { Self::END_STREAM } else { 0 };
        Frame::new(Self::DATA, flags, stream, payload)
    }

    fn rst_stream(stream: u32, code: u32) -> Self {
        Frame::new(Self::RST_STREAM, 0, stream, code.to_be_bytes())
    }

    /// `fields` as an HPACK header block of literals without indexing and without Huffman
    /// coding (RFC 7541 §6.2.2), so any byte can go into a name or value.
    fn block(fields: &[(&str, &str)]) -> Vec<u8> {
        fn string(out: &mut Vec<u8>, s: &str) {
            // A 7-bit prefix integer (RFC 7541 §5.1), then the bytes.
            if s.len() < 127 {
                out.push(s.len() as u8);
            } else {
                out.push(127);
                let mut rest = s.len() - 127;
                while rest >= 128 {
                    out.push((rest % 128) as u8 | 0x80);
                    rest /= 128;
                }
                out.push(rest as u8);
            }
            out.extend_from_slice(s.as_bytes());
        }
        let mut out = Vec::new();
        for (name, value) in fields {
            out.push(0);
            string(&mut out, name);
            string(&mut out, value);
        }
        out
    }

    fn bytes(&self) -> Vec<u8> {
        let len = (self.payload.len() as u32).to_be_bytes();
        let mut out = vec![len[1], len[2], len[3], self.kind, self.flags];
        out.extend_from_slice(&self.stream.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    fn is(&self, kind: u8) -> bool {
        self.kind == kind
    }

    fn ends_stream(&self) -> bool {
        matches!(self.kind, Self::DATA | Self::HEADERS) && self.flags & Self::END_STREAM != 0
            || self.kind == Self::RST_STREAM
    }

    /// The error code of an RST_STREAM or GOAWAY frame.
    fn error_code(&self) -> Option<u32> {
        let code = match self.kind {
            Self::RST_STREAM => self.payload.get(..4)?,
            Self::GOAWAY => self.payload.get(4..8)?,
            _ => return None,
        };
        Some(u32::from_be_bytes(code.try_into().unwrap()))
    }

    /// The `:status` of a response HEADERS frame, when it is the block's first field (it is,
    /// in what Pingora and h2 send): a static-table index, or a literal with the indexed name.
    fn status(&self) -> Option<u16> {
        if self.kind != Self::HEADERS {
            return None;
        }
        let (&first, rest) = self.payload.split_first()?;
        match first {
            0x88 => Some(200),
            0x89 => Some(204),
            0x8a => Some(206),
            0x8b => Some(304),
            0x8c => Some(400),
            0x8d => Some(404),
            0x8e => Some(500),
            // Literal value, name `:status` (static index 8), in the three literal forms.
            0x48 | 0x08 | 0x18 => {
                let (&len, value) = rest.split_first()?;
                let value = value.get(..usize::from(len & 0x7f))?;
                if len & 0x80 == 0 {
                    return std::str::from_utf8(value).ok()?.parse().ok();
                }
                // Huffman-coded digits (RFC 7541 appendix B): `0`-`2` are the 5-bit codes
                // 0-2, `3`-`9` the 6-bit codes 0x19-0x1f.
                let mut bits = value
                    .iter()
                    .flat_map(|b| (0..8).rev().map(move |i| b >> i & 1));
                let mut take = |n: usize| {
                    (0..n).try_fold(0u16, |acc, _| Some(acc << 1 | u16::from(bits.next()?)))
                };
                let mut status = 0;
                for _ in 0..3 {
                    let digit = match take(5)? {
                        code @ 0..=2 => code,
                        code => match code << 1 | take(1)? {
                            code @ 0x19..=0x1f => code - 0x19 + 3,
                            _ => return None,
                        },
                    };
                    status = status * 10 + digit;
                }
                Some(status)
            }
            _ => None,
        }
    }
}

/// What [`H2Client::read_until`] saw.
#[derive(Debug)]
struct Received {
    frames: Vec<Frame>,
    /// The server closed the connection (rather than the wait running out or the wanted
    /// frame arriving).
    closed: bool,
}

impl Received {
    fn of(&self, kind: u8) -> impl Iterator<Item = &Frame> {
        self.frames.iter().filter(move |frame| frame.is(kind))
    }

    /// The GOAWAY's error code and debug data, if one came.
    fn goaway(&self) -> Option<(u32, String)> {
        let frame = self.of(Frame::GOAWAY).next()?;
        let debug = String::from_utf8_lossy(frame.payload.get(8..)?).into_owned();
        Some((frame.error_code()?, debug))
    }

    /// The status of the response on `stream`.
    fn status(&self, stream: u32) -> Option<u16> {
        self.of(Frame::HEADERS)
            .find(|frame| frame.stream == stream)
            .and_then(Frame::status)
    }
}

/// An HTTP/2 connection on which every frame is written and read by hand: a client library
/// would refuse to send most of what the adversarial tests send.
struct H2Client<S>(S);

impl H2Client<TlsStream<TcpStream>> {
    /// A connection to `:443` for the SNI name `sni` that has sent the client preface and an
    /// empty SETTINGS frame.
    async fn connect(h: &Harness, sni: &str) -> Self {
        let mut client = H2Client::handshake(h, sni).await;
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

    /// A TLS connection that negotiated `h2` and has sent nothing yet.
    async fn handshake(h: &Harness, sni: &str) -> Self {
        let connector = h.ca.connector(&[b"h2"]);
        H2Client(h.handshake(&connector, sni).await.unwrap())
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> H2Client<S> {
    /// Writes `frames`; an error means the server has closed the connection.
    async fn send(&mut self, frames: &[Frame]) -> std::io::Result<()> {
        let bytes: Vec<u8> = frames.iter().flat_map(Frame::bytes).collect();
        self.0.write_all(&bytes).await?;
        self.0.flush().await
    }

    /// Reads frames until `done` accepts one, the server closes, or `wait` has passed.
    async fn read_until(
        &mut self,
        wait: Duration,
        mut done: impl FnMut(&Frame) -> bool,
    ) -> Received {
        let deadline = tokio::time::Instant::now() + wait;
        let mut frames = Vec::new();
        loop {
            let frame = async {
                let mut head = [0; 9];
                self.0.read_exact(&mut head).await?;
                let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
                let mut payload = vec![0; len];
                self.0.read_exact(&mut payload).await?;
                let stream = u32::from_be_bytes([head[5], head[6], head[7], head[8]]);
                std::io::Result::Ok(Frame::new(head[3], head[4], stream & 0x7fff_ffff, payload))
            };
            match tokio::time::timeout_at(deadline, frame).await {
                Err(_) => {
                    return Received {
                        frames,
                        closed: false,
                    };
                }
                Ok(Err(_)) => {
                    return Received {
                        frames,
                        closed: true,
                    };
                }
                Ok(Ok(frame)) => {
                    let stop = done(&frame);
                    frames.push(frame);
                    if stop {
                        return Received {
                            frames,
                            closed: false,
                        };
                    }
                }
            }
        }
    }

    /// Reads until the server closes the connection or `wait` has passed.
    async fn read_to_end(&mut self, wait: Duration) -> Received {
        self.read_until(wait, |_| false).await
    }
}

// ---------------------------------------------------------------------------------------------
// Routing and built-in responses

#[tokio::test]
async fn routes_by_host_and_prefix_and_rejects_the_rest() {
    let h = Harness::start().await;

    let reply = h.get("app.example.com", "/hello?x=1").await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["path"], "/hello?x=1");

    let reply = h.get("api.example.com", "/v1/users").await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["path"], "/v1/users");

    // Known host, no matching prefix: `/v1` does not match `/v10` or `/`.
    for target in ["/", "/v10", "/other"] {
        let reply = h.get("api.example.com", target).await;
        assert_eq!(reply.status, 404, "{target}");
        assert_eq!(reply.text(), "404 Not Found\n");
    }

    // A host with a certificate but no routes.
    let reply = h.get("noroute.example.com", "/").await;
    assert_eq!(reply.status, 404);
    assert_eq!(
        reply.header("content-type"),
        Some("text/plain; charset=utf-8")
    );

    // Host matching ignores case, port and a trailing dot.
    let reply = h.get("APP.example.com.:8443", "/x").await;
    assert_eq!(reply.status, 200);
}

#[tokio::test]
async fn a_service_in_its_gap_answers_503_with_retry_after() {
    let h = Harness::start().await;
    let reply = h.get("gap.example.com", "/").await;
    assert_eq!(reply.status, 503);
    assert_eq!(reply.header("retry-after"), Some("7"));
    assert_eq!(reply.text(), "503 Service Unavailable\n");
}

#[tokio::test]
async fn a_service_answers_503_during_its_gap_and_200_after_the_swap() {
    let h = Harness::start().await;
    let reply = h.get("gap.example.com", "/during").await;
    assert_eq!(reply.status, 503);
    assert_eq!(reply.header("retry-after"), Some("7"));

    // The deploy's commit points the service at its new, healthy container.
    h.swap(
        "gapped",
        Arc::new(Upstream::loopback(
            h.web_port,
            name("site"),
            name("gapped"),
            2,
        )),
    );
    let reply = h.get("gap.example.com", "/after").await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.header("retry-after"), None);
    assert_eq!(reply.json()["path"], "/after");
}

#[tokio::test]
async fn reaches_a_unix_socket_target() {
    let h = Harness::start().await;
    let reply = h.get("admin.example.com", "/healthz").await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["path"], "/healthz");
    assert_eq!(echoed(&reply, "host"), ["admin.example.com"]);
}

#[tokio::test]
async fn the_admin_host_reaches_the_admin_app() {
    let h = Harness::start().await;
    let reply = h.get("hoster.example.com", "/healthz").await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.text(), "ok\n");
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert!(
        reply
            .header("content-security-policy")
            .is_some_and(|csp| csp.starts_with("default-src 'self'")),
        "{:?}",
        reply.headers
    );

    // The UI, served through the proxy: a signed-out visitor is sent to log in.
    let reply = h.get("hoster.example.com", "/").await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    assert_eq!(reply.header("location"), Some("/auth/login"));

    // `/hooks/*` goes to the same app: the webhook itself asks for a token.
    let head = "POST /hooks/deploy HTTP/1.1\r\nHost: hoster.example.com\r\n\
                Content-Length: 2\r\nConnection: close\r\n\r\n{}";
    let reply = h.request(head).await;
    assert_eq!(reply.status, 401, "{}", reply.text());
    assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
    assert!(
        reply.header("content-security-policy").is_some(),
        "{reply:?}"
    );
}

#[tokio::test]
async fn the_admin_app_builds_urls_and_checks_origins_through_the_proxy() {
    let h = Harness::start().await;

    // The OAuth callback is an absolute URL on the admin host.
    let reply = h.get("hoster.example.com", "/auth/github/login").await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    let location = reply.header("location").expect("redirect to GitHub");
    assert!(
        location
            .contains("redirect_uri=https%3A%2F%2Fhoster.example.com%2Fauth%2Fgithub%2Fcallback"),
        "{location}"
    );

    // A signed-in browser gets the UI.
    let browser = h.admin.sign_in().await;
    let (cookie, csrf) = (&browser.cookie, &browser.csrf);
    let home = format!(
        "GET / HTTP/1.1\r\nHost: hoster.example.com\r\nCookie: {cookie}\r\n\
         Connection: close\r\n\r\n"
    );
    let reply = h.request(&home).await;
    assert_eq!(reply.status, 200, "{}", reply.text());

    // A POST from another origin is refused by the app's CSRF check, even with the token.
    let logout = |origin: &str, fetch_site: &str| {
        format!(
            "POST /auth/logout HTTP/1.1\r\nHost: hoster.example.com\r\nCookie: {cookie}\r\n\
             Origin: {origin}\r\nSec-Fetch-Site: {fetch_site}\r\nX-CSRF-Token: {csrf}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
    };
    let reply = h
        .request(&logout("https://evil.example.com", "cross-site"))
        .await;
    assert_eq!(reply.status, 403, "{}", reply.text());
    assert_eq!(reply.text(), "cross-origin request refused\n");

    // The same POST from the admin origin passes it and ends the session.
    let reply = h
        .request(&logout("https://hoster.example.com", "same-origin"))
        .await;
    assert_eq!(reply.status, 303, "{}", reply.text());
    assert_eq!(reply.header("location"), Some("/auth/login"));
    let reply = h.request(&home).await;
    assert_eq!(reply.status, 303, "signed out: {}", reply.text());
}

#[tokio::test]
async fn a_unix_socket_target_sees_the_forwarded_headers() {
    let h = Harness::start().await;
    let reply = h.get("admin.example.com", "/whoami").await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(echoed(&reply, "host"), ["admin.example.com"]);
    assert_eq!(echoed(&reply, "x-forwarded-host"), ["admin.example.com"]);
    assert_eq!(echoed(&reply, "x-forwarded-proto"), ["https"]);
    assert_eq!(echoed(&reply, "x-forwarded-for"), ["127.0.0.1"]);

    // A browser's `Origin` reaches the app untouched, for its CSRF check.
    let reply = h
        .request(
            "POST /whoami HTTP/1.1\r\nHost: admin.example.com\r\n\
             Origin: https://admin.example.com\r\nSec-Fetch-Site: same-origin\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    assert_eq!(echoed(&reply, "origin"), ["https://admin.example.com"]);
    assert_eq!(echoed(&reply, "sec-fetch-site"), ["same-origin"]);
}

/// Reads `client` until `needle` has arrived, failing after `within`.
async fn read_until<S: AsyncRead + AsyncWrite + Unpin>(
    client: &mut Client<S>,
    needle: &str,
    within: Duration,
) -> std::io::Result<String> {
    let mut seen = String::new();
    let mut buf = [0; 256];
    tokio::time::timeout(within, async {
        while !seen.contains(needle) {
            let n = client.0.read(&mut buf).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        Ok(seen)
    })
    .await
    .map_err(|_| std::io::ErrorKind::TimedOut)?
}

#[tokio::test]
async fn admin_sse_streams_outlive_the_default_read_timeout() {
    let short = Timeouts {
        read: Duration::from_millis(300),
        ..Timeouts::default()
    };
    let h = &Harness::start_with(short).await;

    let open = |host: &'static str| async move {
        let mut client = h.connect(host).await;
        client
            .write(
                format!("GET /sse HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await;
        let (status, _) = client.head().await;
        assert_eq!(status, 200, "{host}");
        read_until(&mut client, "data: first", Duration::from_secs(5))
            .await
            .unwrap_or_else(|e| panic!("{host}: first event: {e}"));
        client
    };

    // A stack route under the short default: the quiet stream is cut.
    let mut app = open("app.example.com").await;
    let cut = read_until(&mut app, "data: second", Duration::from_secs(5)).await;
    assert!(cut.is_err(), "{cut:?}");

    // The admin route keeps a stream open across a silence longer than that default.
    let mut admin = open("admin.example.com").await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    // Both handlers wait on the same release; the cut one may not have noticed yet.
    h.state.sse_release.notify_waiters();
    read_until(&mut admin, "data: second", Duration::from_secs(5))
        .await
        .expect("the stream is still open");
}

#[tokio::test]
async fn connect_failure_is_a_502_page() {
    let h = Harness::start().await;
    let reply = h.get("down.example.com", "/").await;
    assert_eq!(reply.status, 502);
    assert_eq!(reply.text(), "502 Bad Gateway\n");
}

/// The statistics of the web UI (ADR-0045): each request counted against its route by status
/// class, upstream failures counted as such, and unmatched requests as unrouted.
#[tokio::test]
async fn requests_are_counted_in_the_route_statistics() {
    let h = Harness::start().await;
    assert_eq!(h.get("app.example.com", "/hello").await.status, 200);
    assert_eq!(h.get("APP.example.com", "/again").await.status, 200);
    assert_eq!(h.get("app.example.com", "/api/x").await.status, 200);
    assert_eq!(h.get("down.example.com", "/").await.status, 502);
    assert_eq!(h.get("api.example.com", "/other").await.status, 404);

    // The `logging` hook runs after the response went out.
    let drained = Drained::until(&h.traffic, |d| {
        d.routes.values().map(|r| r.statuses.total()).sum::<u64>() >= 5
    })
    .await;
    let root = drained.route("app.example.com/");
    assert_eq!(root.statuses.get(StatusClass::Success), 2);
    assert_eq!(root.latency.samples(), 2);
    assert!(root.latency.p95().is_some());
    assert_eq!(h.traffic.in_flight().hosted(), 0);
    assert_eq!(drained.route("app.example.com/api").statuses.total(), 1);
    let down = drained.route("down.example.com/");
    assert_eq!(down.statuses.get(StatusClass::ServerError), 1);
    assert_eq!(down.upstream_errors, 1);
    assert_eq!(
        drained.route("").statuses.get(StatusClass::ClientError),
        1,
        "unrouted"
    );
}

/// What the traffic counters drained, added up per `host` + `prefix` (`""` for unrouted).
#[derive(Debug, Default)]
struct Drained {
    routes: BTreeMap<String, DrainedRoute>,
}

#[derive(Debug, Default)]
struct DrainedRoute {
    statuses: StatusCounts,
    upstream_errors: u64,
    latency: Histogram,
}

impl Drained {
    /// Drains `traffic` until `done` holds of everything drained, for up to five seconds.
    async fn until(traffic: &Traffic, done: impl Fn(&Drained) -> bool) -> Drained {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut drained = Drained::default();
        loop {
            for delta in traffic.drain() {
                let route = drained
                    .routes
                    .entry(format!("{}{}", delta.host, delta.prefix))
                    .or_default();
                route.statuses += delta.statuses;
                route.upstream_errors += delta.upstream_errors;
                route.latency += delta.latency;
            }
            if done(&drained) || Instant::now() > deadline {
                return drained;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn route(&self, route: &str) -> &DrainedRoute {
        self.routes
            .get(route)
            .unwrap_or_else(|| panic!("{route} in {self:?}"))
    }
}

#[tokio::test]
async fn a_request_with_a_body_is_not_retried() {
    let h = Harness::start().await;
    let reply = h
        .request(
            "POST /submit HTTP/1.1\r\nHost: reset.example.com\r\nContent-Length: 5\r\n\
             Connection: close\r\n\r\nhello",
        )
        .await;
    assert_eq!(reply.status, 502);
    assert_eq!(h.resets.load(Ordering::SeqCst), 1, "exactly one attempt");
}

/// On a pooled connection the upstream closed, a bodyless `GET` is replayed on a fresh
/// connection, but a `POST` whose body was already sent is not (ADR-0011). Pingora's default
/// would replay the `POST` too, from its retry buffer.
#[tokio::test]
async fn only_bodyless_idempotent_requests_are_retried_on_a_dead_pooled_connection() {
    let h = Harness::start().await;
    let count = || h.flaky_requests.load(Ordering::SeqCst);
    let mut client = h.connect("flaky.example.com").await;
    let mut send = async |head: &str| {
        client.write(head.as_bytes()).await;
        client.reply().await
    };

    // Pools a connection that will close on its next request.
    let get = "GET / HTTP/1.1\r\nHost: flaky.example.com\r\n\r\n";
    assert_eq!(send(get).await.status, 200);
    assert_eq!(count(), 1);

    let post = "POST / HTTP/1.1\r\nHost: flaky.example.com\r\nContent-Length: 5\r\n\r\nhello";
    let reply = send(post).await;
    assert_eq!(reply.status, 502, "{}", reply.text());
    assert_eq!(
        count(),
        2,
        "the POST reached the upstream once and was not replayed"
    );

    let mut client = h.connect("flaky.example.com").await;
    let mut send = async |head: &str| {
        client.write(head.as_bytes()).await;
        client.reply().await
    };
    assert_eq!(send(get).await.status, 200);
    assert_eq!(count(), 3);
    let reply = send(get).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(count(), 5, "the GET was replayed on a fresh connection");
}

// ---------------------------------------------------------------------------------------------
// TLS on :443 (ADR-0014, ADR-0032, ADR-0033)

#[tokio::test]
async fn serves_the_certificate_for_each_sni_name() {
    let h = Harness::start().await;
    for host in ["app.example.com", "api.example.com", "admin.example.com"] {
        let stream = h.handshake(&h.tls, host).await.unwrap();
        let served = stream.get_ref().1.peer_certificates().unwrap()[0].clone();
        let expected = h.certs.get(host).unwrap();
        assert_eq!(&served, expected.end_entity_cert().unwrap(), "{host}");
    }
}

#[tokio::test]
async fn an_unknown_sni_name_fails_the_handshake() {
    let h = Harness::start().await;
    let err = h
        .handshake(&h.tls, "unknown.example.com")
        .await
        .expect_err("no default certificate");
    // The server aborts with an alert; the client never sees a certificate to reject.
    assert!(err.to_string().contains("alert"), "{err}");
}

#[tokio::test]
async fn certificates_inserted_later_are_served_without_a_restart() {
    let h = Harness::start().await;
    assert!(h.handshake(&h.tls, "late.example.com").await.is_err());
    let ca = TestCa::new();
    let (chain, key) = ca.issue("late.example.com");
    h.certs.insert("late.example.com", chain, key).unwrap();
    let stream = h
        .handshake(&ca.connector(&[b"http/1.1"]), "late.example.com")
        .await
        .expect("the new certificate is served");
    drop(stream);
}

#[tokio::test]
async fn h2_is_negotiated_and_served() {
    let h = Harness::start().await;
    let h2 = h.ca.connector(&[b"h2", b"http/1.1"]);
    let mut stream = h.handshake(&h2, "app.example.com").await.unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));

    // The connection preface and an empty SETTINGS frame get the server's SETTINGS back.
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let mut frame = [0; 9];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut frame))
        .await
        .expect("server SETTINGS")
        .unwrap();
    assert_eq!(frame[3], 0x04, "SETTINGS frame type: {frame:?}");

    let h1 = h.ca.connector(&[b"http/1.1"]);
    let stream = h.handshake(&h1, "app.example.com").await.unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
}

/// Requests over h2 are routed like h1 ones: the SNI recorded on the connection is visible to
/// every stream, so a matching `:authority` is proxied rather than answered with 421.
#[tokio::test]
async fn h2_requests_are_routed() {
    let h = Harness::start().await;
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(h.ca.client_config(&[b"h2"]))
        .resolve("app.example.com", h.proxy)
        .resolve("api.example.com", h.proxy)
        .build()
        .unwrap();
    let port = h.proxy.port();
    for url in [
        format!("https://app.example.com:{port}/"),
        format!("https://api.example.com:{port}/v1/x"),
    ] {
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(response.version(), reqwest::Version::HTTP_2, "{url}");
        assert_eq!(response.status(), 200, "{url}");
    }
}

#[tokio::test]
async fn a_host_that_is_not_the_sni_name_is_misdirected() {
    let h = Harness::start().await;
    let head = "GET /v1/x HTTP/1.1\r\nHost: api.example.com\r\nConnection: close\r\n\r\n";
    let reply = h.request_with_sni("app.example.com", head).await;
    assert_eq!(reply.status, 421, "{}", reply.text());
    assert_eq!(reply.text(), "421 Misdirected Request\n");

    // Port, case and a trailing dot do not make a mismatch.
    let head = "GET /v1/x HTTP/1.1\r\nHost: API.example.com.:443\r\nConnection: close\r\n\r\n";
    let reply = h.request_with_sni("api.example.com", head).await;
    assert_eq!(reply.status, 200, "{}", reply.text());

    // On a kept-alive connection, every request is checked.
    let mut client = h.connect("app.example.com").await;
    client
        .write(b"GET / HTTP/1.1\r\nHost: app.example.com\r\n\r\n")
        .await;
    assert_eq!(client.reply().await.status, 200);
    client
        .write(b"GET /v1/x HTTP/1.1\r\nHost: api.example.com\r\nConnection: close\r\n\r\n")
        .await;
    assert_eq!(client.reply().await.status, 421);
}

// ---------------------------------------------------------------------------------------------
// Port 80 (ADR-0015, ADR-0032)

#[tokio::test]
async fn plain_http_redirects_known_hosts_and_keeps_path_and_query() {
    let h = Harness::start().await;
    let reply = h.get_http("app.example.com", "/a/b?c=d&e=%20f").await;
    assert_eq!(reply.status, 308, "{}", reply.text());
    assert_eq!(
        reply.header("location"),
        Some("https://app.example.com/a/b?c=d&e=%20f")
    );

    let reply = h.get_http("Admin.Example.com:80", "/").await;
    assert_eq!(reply.status, 308);
    assert_eq!(reply.header("location"), Some("https://admin.example.com/"));

    let reply = h.get_http("evil.example.com", "/x").await;
    assert_eq!(reply.status, 404, "no open redirect");
    assert_eq!(reply.header("location"), None);

    // A certificate alone does not make a host known on :80.
    let reply = h.get_http("noroute.example.com", "/").await;
    assert_eq!(reply.status, 404);
}

#[tokio::test]
async fn plain_http_answers_acme_challenges_for_any_host() {
    let h = Harness::start().await;
    let path = "/.well-known/acme-challenge/tok-1_A";
    let reply = h.get_http("new.example.com", path).await;
    assert_eq!(reply.status, 404, "no challenge yet");

    h.challenges.insert("tok-1_A", "tok-1_A.thumbprint");
    for host in ["new.example.com", "app.example.com"] {
        let reply = h.get_http(host, path).await;
        assert_eq!(reply.status, 200, "{host}");
        assert_eq!(reply.header("content-type"), Some("text/plain"));
        assert_eq!(reply.text(), "tok-1_A.thumbprint");
    }

    h.challenges.remove("tok-1_A");
    assert_eq!(h.get_http("new.example.com", path).await.status, 404);
}

// ---------------------------------------------------------------------------------------------
// Request rewriting (ADR-0010)

#[tokio::test]
async fn prefix_is_kept_or_stripped_and_announced() {
    let h = Harness::start().await;

    let reply = h.get("app.example.com", "/keep/a?b=c").await;
    assert_eq!(reply.json()["path"], "/keep/a?b=c");
    assert_eq!(echoed(&reply, "x-forwarded-prefix"), ["/keep"]);

    let reply = h.get("app.example.com", "/api/users?x=1").await;
    assert_eq!(reply.json()["path"], "/users?x=1");
    assert_eq!(echoed(&reply, "x-forwarded-prefix"), ["/api"]);

    let reply = h.get("app.example.com", "/api").await;
    assert_eq!(reply.json()["path"], "/");

    // `/apiary` is not under `/api`; it falls through to `/`, which sends no prefix.
    let reply = h.get("app.example.com", "/apiary").await;
    assert_eq!(reply.json()["path"], "/apiary");
    assert!(echoed(&reply, "x-forwarded-prefix").is_empty());
}

#[tokio::test]
async fn forwarding_headers_are_set_and_client_values_overwritten() {
    let h = Harness::start().await;
    let reply = h
        .request(
            "GET /x HTTP/1.1\r\nHost: App.example.com:8443\r\n\
             X-Forwarded-For: 198.51.100.1\r\nX-Forwarded-Proto: https\r\n\
             X-Forwarded-Host: evil.example.com\r\nX-Forwarded-Prefix: /evil\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        echoed(&reply, "host"),
        ["App.example.com:8443"],
        "Host unchanged"
    );
    assert_eq!(
        echoed(&reply, "x-forwarded-for"),
        ["198.51.100.1, 127.0.0.1"]
    );
    assert_eq!(echoed(&reply, "x-forwarded-proto"), ["https"]);
    assert_eq!(echoed(&reply, "x-forwarded-host"), ["App.example.com:8443"]);
    assert!(echoed(&reply, "x-forwarded-prefix").is_empty());
}

#[tokio::test]
async fn absolute_form_targets_route_by_their_authority() {
    let h = Harness::start().await;
    let reply = h
        .request(
            "GET http://app.example.com/api/abs?q=1 HTTP/1.1\r\nHost: app.example.com\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["target"], "/abs?q=1");
    assert_eq!(echoed(&reply, "host"), ["app.example.com"]);

    // Unstripped, the upstream still gets origin-form, not the client's absolute URL.
    let reply = h
        .request(
            "GET http://app.example.com/keep/abs?q=1 HTTP/1.1\r\nHost: app.example.com\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["target"], "/keep/abs?q=1");

    // A Host that disagrees with the target's authority is refused before routing.
    let reply = h
        .request(
            "GET http://api.example.com/v1/x HTTP/1.1\r\nHost: app.example.com\r\n\
             Connection: close\r\n\r\n",
        )
        .await;
    assert_eq!(reply.status, 400, "{}", reply.text());
}

#[tokio::test]
async fn very_long_paths_are_routed_and_stripped() {
    let h = Harness::start().await;
    let tail = "a".repeat(8 * 1024);
    let reply = h.get("app.example.com", &format!("/api/{tail}")).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["path"], format!("/{tail}"));
}

// ---------------------------------------------------------------------------------------------
// Timeouts (ADR-0011)

#[tokio::test]
async fn read_timeouts_are_per_route() {
    let h = Harness::start().await;

    let started = Instant::now();
    let reply = h.get("slow.example.com", "/slow/2000").await;
    assert_eq!(reply.status, 504, "{}", reply.text());
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );

    // The same upstream under the default read timeout waits for the answer.
    let reply = h.get("app.example.com", "/slow/600").await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.text(), "late");
}

// ---------------------------------------------------------------------------------------------
// Streaming (ADR-0011)

#[tokio::test]
async fn server_sent_events_are_not_buffered() {
    let h = Harness::start().await;
    let mut client = h.connect("app.example.com").await;
    client
        .write(b"GET /api/sse HTTP/1.1\r\nHost: app.example.com\r\nConnection: close\r\n\r\n")
        .await;
    let (status, headers) = client.head().await;
    assert_eq!(status, 200);
    assert!(
        headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("content-type") && v == "text/event-stream")
    );
    assert_eq!(h.web.inflight(), 1, "the open stream counts as in flight");

    // The first event arrives while the upstream is still holding the second back.
    let mut seen = String::new();
    let mut buf = [0; 256];
    tokio::time::timeout(Duration::from_secs(5), async {
        while !seen.contains("data: first") {
            let n = client.0.read(&mut buf).await.unwrap();
            assert!(n > 0, "stream ended early");
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    })
    .await
    .expect("first event is forwarded before the stream ends");
    assert!(!seen.contains("second"));

    // The stream stays open far longer than its header took to arrive.
    const OPEN: Duration = Duration::from_millis(1000);
    tokio::time::sleep(OPEN).await;
    h.state.sse_release.notify_one();
    let mut rest = Vec::new();
    client.0.read_to_end(&mut rest).await.unwrap();
    assert!(String::from_utf8_lossy(&rest).contains("data: second"));

    // The guard is dropped when the request is done.
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.web.inflight() != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.web.inflight(), 0);

    // Its latency in the statistics is the time to the response header, not its lifetime.
    let drained = Drained::until(&h.traffic, |d| {
        d.routes
            .get("app.example.com/api")
            .is_some_and(|r| r.latency.samples() == 1)
    })
    .await;
    let latency = drained.route("app.example.com/api").latency;
    assert_eq!(latency.samples(), 1, "stream not counted: {drained:?}");
    assert!(latency.p95().unwrap() < OPEN / 2, "{latency:?}");
}

// ---------------------------------------------------------------------------------------------
// Draining (architecture §3.2, §5.3)

#[tokio::test]
async fn drain_waits_for_a_request_in_flight_across_a_table_swap() {
    let h = Harness::start().await;
    let old = Arc::clone(&h.web);
    let sent = Instant::now();
    let slow = tokio::spawn({
        let mut client = h.connect("app.example.com").await;
        async move {
            client
                .write(
                    b"GET /slow/800 HTTP/1.1\r\nHost: app.example.com\r\nConnection: close\r\n\r\n",
                )
                .await;
            client.reply().await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while old.inflight() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(old.inflight(), 1, "the slow request is in flight");

    // The new generation serves from the same app on the same port; only the Arc differs.
    let new = Arc::new(Upstream::loopback(h.web_port, name("site"), name("web"), 2));
    h.swap_web(Arc::clone(&new));
    let table = h.routes.load();
    let Target::Loopback(routed) = &table.lookup("app.example.com", "/").unwrap().target else {
        panic!("expected a loopback target");
    };
    assert!(Arc::ptr_eq(routed, &new));

    // A request after the swap goes to the new generation and does not hold up the drain.
    let reply = h.get("app.example.com", "/fresh").await;
    assert_eq!(reply.status, 200);
    assert_eq!(old.inflight(), 1);

    let started = Instant::now();
    let outcome = old
        .drain(Duration::from_millis(50), Duration::from_secs(10))
        .await;
    assert_eq!(outcome, DrainOutcome::Drained);
    assert!(
        sent.elapsed() >= Duration::from_millis(800),
        "drain returned after {:?}, before the 800 ms request finished",
        sent.elapsed()
    );
    assert!(started.elapsed() >= Duration::from_millis(50));
    let reply = slow.await.unwrap();
    assert_eq!(reply.status, 200);
    assert_eq!(reply.text(), "late");
    assert_eq!(old.inflight(), 0);
}

#[tokio::test]
async fn drain_times_out_while_a_request_is_still_in_flight() {
    let h = Harness::start().await;
    let slow = tokio::spawn({
        let mut client = h.connect("app.example.com").await;
        async move {
            client
                .write(b"GET /slow/1500 HTTP/1.1\r\nHost: app.example.com\r\nConnection: close\r\n\r\n")
                .await;
            client.reply().await
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.web.inflight() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    h.swap_web(Arc::new(Upstream::loopback(
        h.web_port,
        name("site"),
        name("web"),
        2,
    )));
    let outcome = h
        .web
        .drain(Duration::from_millis(10), Duration::from_millis(200))
        .await;
    assert_eq!(outcome, DrainOutcome::TimedOut(1));
    assert_eq!(slow.await.unwrap().status, 200);
}

#[tokio::test]
async fn failed_requests_release_the_inflight_guard() {
    let h = Harness::start().await;
    let table = h.routes.load_full();
    let Target::Loopback(reset) = &table.lookup("reset.example.com", "/").unwrap().target else {
        panic!("expected a loopback target");
    };
    let reply = h.get("reset.example.com", "/").await;
    assert_eq!(reply.status, 502, "{}", reply.text());
    let outcome = reset.drain(Duration::ZERO, Duration::from_secs(4)).await;
    assert_eq!(outcome, DrainOutcome::Drained);
    assert_eq!(
        h.resets.load(Ordering::SeqCst),
        1,
        "the request did reach the upstream"
    );
}

#[tokio::test]
async fn client_abort_releases_the_inflight_guard() {
    let h = Harness::start().await;
    let mut client = h.connect("app.example.com").await;
    client
        .write(b"GET /slow/5000 HTTP/1.1\r\nHost: app.example.com\r\n\r\n")
        .await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.web.inflight() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(h.web.inflight(), 1);
    drop(client);
    let outcome = h.web.drain(Duration::ZERO, Duration::from_secs(4)).await;
    assert_eq!(
        outcome,
        DrainOutcome::Drained,
        "the aborted request is released"
    );
}

#[tokio::test]
async fn websockets_pass_through() {
    let h = Harness::start().await;
    let mut client = h.connect("app.example.com").await;
    client
        .write(
            b"GET /api/ws HTTP/1.1\r\nHost: app.example.com\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
    let (status, headers) = client.head().await;
    assert_eq!(status, 101, "{headers:?}");

    // One masked text frame; the echo comes back unmasked.
    let payload = b"hello";
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
    client.write(&frame).await;

    let mut echo = [0; 7];
    tokio::time::timeout(Duration::from_secs(5), client.0.read_exact(&mut echo))
        .await
        .expect("echo frame")
        .unwrap();
    assert_eq!(echo[..2], [0x81, 5]);
    assert_eq!(&echo[2..], payload);
}

/// Resident set size of this process.
fn rss() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

/// Samples [`rss`] until dropped; yields the peak growth over the starting value.
struct RssWatch {
    peak: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
    base: usize,
}

impl RssWatch {
    fn start() -> Self {
        let base = rss();
        let peak = Arc::new(AtomicUsize::new(base));
        let sampler = Arc::clone(&peak);
        let task = tokio::spawn(async move {
            loop {
                sampler.fetch_max(rss(), Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        Self { peak, task, base }
    }

    fn growth(self) -> usize {
        self.task.abort();
        self.peak
            .load(Ordering::Relaxed)
            .max(rss())
            .saturating_sub(self.base)
    }
}

/// Streams `bytes` up and then down through the proxy and checks memory stays far below it.
async fn stream_through(bytes: usize) {
    let h = Harness::start().await;
    let watch = RssWatch::start();

    let mut client = h.connect("app.example.com").await;
    client
        .write(
            format!(
                "POST /api/upload HTTP/1.1\r\nHost: app.example.com\r\nContent-Length: {bytes}\r\n\
                 Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await;
    let mut left = bytes;
    while left > 0 {
        let n = left.min(CHUNK);
        client.write(&ZEROS[..n]).await;
        left -= n;
    }
    let reply = client.reply().await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.text(),
        bytes.to_string(),
        "upstream got the whole upload"
    );

    let mut client = h.connect("app.example.com").await;
    client
        .write(
            format!(
                "GET /api/download/{bytes} HTTP/1.1\r\nHost: app.example.com\r\n\
                 Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await;
    let (status, headers) = client.head().await;
    assert_eq!(status, 200);
    let mut received = 0;
    client.body(&headers, |chunk| received += chunk.len()).await;
    assert_eq!(received, bytes, "client got the whole download");

    // Buffering either body would grow memory by about `bytes`; streaming stays at a few
    // chunks per connection. The slack covers tests running in parallel in this process.
    let growth = watch.growth();
    let limit = (bytes / 2).min(MEMORY_SLACK);
    assert!(
        growth < limit,
        "memory grew by {growth} bytes while streaming {bytes}"
    );
}

const MEMORY_SLACK: usize = 128 * 1024 * 1024;

/// 1 GiB up and 1 GiB down with bounded memory; `TEST_HOSTER_PROXY_STREAM_BYTES` overrides the size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_gib_streams_with_bounded_memory() {
    let bytes = std::env::var("TEST_HOSTER_PROXY_STREAM_BYTES")
        .ok()
        .and_then(|b| b.parse().ok())
        .unwrap_or(1024 * 1024 * 1024);
    stream_through(bytes).await;
}

// ---------------------------------------------------------------------------------------------
// Access log

/// The access log line of the one request to `path`, waiting for Pingora's `logging` hook,
/// which runs after the response went out.
async fn access_line(path: &str) -> Value {
    let log = crate::logging::capture::global();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let lines: Vec<Value> = log
            .json()
            .into_iter()
            .filter(|line| line["target"] == "hoster::access" && line["path"] == path)
            .collect();
        match lines.as_slice() {
            [line] => return line.clone(),
            [] if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(20)).await,
            _ => panic!("expected one access log line for {path}, got {lines:?}"),
        }
    }
}

#[tokio::test]
async fn every_request_that_matched_a_route_gets_an_access_log_line() {
    crate::logging::capture::global();
    let h = Harness::start().await;

    let reply = h.get("app.example.com", "/access/proxied?secret=1").await;
    assert_eq!(reply.status, 200);
    let line = access_line("/access/proxied").await;
    assert_eq!(line["target"], "hoster::access");
    assert_eq!(line["level"], "DEBUG");
    assert_eq!(line["message"], "request");
    assert_eq!(line["host"], "app.example.com");
    assert_eq!(line["method"], "GET");
    assert_eq!(line["route"], "/");
    assert_eq!(line["stack"], "site");
    assert_eq!(line["service"], "web");
    assert_eq!(line["generation"], 1);
    assert_eq!(line["upstream"], format!("127.0.0.1:{}", h.web_port));
    assert_eq!(line["status"], 200);
    assert_eq!(line["client"], "127.0.0.1");
    assert_eq!(line["bytes"], reply.body.len());
    assert!(line["duration_ms"].as_f64().unwrap() >= 0.0, "{line}");
    assert!(line.get("error").is_none(), "{line}");
    assert!(!line.to_string().contains("secret"), "{line}");

    let reply = h.get("gap.example.com", "/access/gap").await;
    assert_eq!(reply.status, 503);
    let line = access_line("/access/gap").await;
    assert_eq!(line["route"], "/");
    assert_eq!(line["service"], "gapped");
    assert_eq!(line["status"], 503);
    assert!(line.get("upstream").is_none(), "{line}");

    let reply = h.get("down.example.com", "/access/down").await;
    assert_eq!(reply.status, 502);
    let line = access_line("/access/down").await;
    assert_eq!(line["service"], "down");
    assert_eq!(line["status"], 502);
    assert!(
        line["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{line}"
    );

    let reply = h.get("admin.example.com", "/access/admin").await;
    assert_eq!(reply.status, 200);
    let line = access_line("/access/admin").await;
    assert_eq!(line["stack"], "hoster");
    assert_eq!(line["service"], "admin UI");
    assert!(
        line["upstream"]
            .as_str()
            .is_some_and(|u| u.starts_with("unix:") && u.ends_with("admin.sock")),
        "{line}"
    );
}

// ---------------------------------------------------------------------------------------------
// Request rows in the log store (ADR-0067 §5)

/// A routed request's row: the route's host spelling, the path as the client sent it without
/// the query, both timings, and an id with the shape of a W3C trace id, new per request.
#[tokio::test]
async fn a_routed_request_is_offered_as_a_request_row() {
    let h = Harness::start().await;
    let before = crate::store::Timestamp::now();
    let reply = h.get("App.Example.com", "/api/rows?token=secret").await;
    assert_eq!(reply.status, 200);
    let row = h.next_request().await;
    assert!(row.time >= before && row.time <= crate::store::Timestamp::now());
    assert_eq!(row.host, "app.example.com");
    assert_eq!(row.prefix, "/api");
    assert_eq!(row.stack, "site");
    assert_eq!(row.service, "web");
    assert_eq!(row.method, "GET");
    assert_eq!(row.path, "/api/rows", "the query is never stored");
    assert_eq!(row.status, 200);
    let to_header = row.to_header.expect("the upstream answered");
    assert!(to_header <= row.total, "{row:?}");
    assert_eq!(row.bytes, reply.body.len() as u64);
    assert_eq!(row.client, std::net::IpAddr::from([127, 0, 0, 1]));
    assert!(!row.upstream_failed);
    let id = row.request_id.to_string();
    assert_eq!(id.len(), 32);
    assert!(
        id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{id}"
    );

    assert_eq!(h.get("app.example.com", "/again").await.status, 200);
    let again = h.next_request().await;
    assert_eq!(again.prefix, "/");
    assert_ne!(again.request_id, row.request_id, "one id per request");
}

/// A request whose upstream cannot be reached: the status the client got, no response header
/// from the upstream, marked as an upstream failure. A service in its stop-first gap is the
/// stack's too, answered by hoster without an upstream failure.
#[tokio::test]
async fn failed_and_gapped_requests_are_rows_of_their_stack() {
    let h = Harness::start().await;
    assert_eq!(h.get("down.example.com", "/rows/down").await.status, 502);
    let row = h.next_request().await;
    assert_eq!(row.service, "down");
    assert_eq!(row.path, "/rows/down");
    assert_eq!(row.status, 502);
    assert_eq!(row.to_header, None);
    assert!(row.upstream_failed);

    assert_eq!(h.get("gap.example.com", "/rows/gap").await.status, 503);
    let row = h.next_request().await;
    assert_eq!(row.service, "gapped");
    assert_eq!(row.status, 503);
    assert_eq!(row.to_header, None);
    assert!(!row.upstream_failed);
}

/// What hoster answers without a stack is not stored: an unknown host, an unknown prefix, a
/// host that is not the SNI name, and hoster's own reserved hosts.
#[tokio::test]
async fn requests_without_a_stack_are_not_stored() {
    let h = Harness::start().await;
    assert_eq!(h.get("noroute.example.com", "/").await.status, 404);
    assert_eq!(h.get("api.example.com", "/other").await.status, 404);
    let head = "GET / HTTP/1.1\r\nHost: api.example.com\r\nConnection: close\r\n\r\n";
    assert_eq!(
        h.request_with_sni("app.example.com", head).await.status,
        421
    );
    assert_eq!(h.get("admin.example.com", "/").await.status, 200);

    // The hook offers a row before it counts the request, so once all four are counted any
    // row would have been offered.
    let drained = Drained::until(&h.traffic, |d| {
        d.routes.values().map(|r| r.statuses.total()).sum::<u64>() >= 4
    })
    .await;
    let counted: u64 = drained.routes.values().map(|r| r.statuses.total()).sum();
    assert_eq!(counted, 4, "{drained:?}");
    assert_eq!(h.tap.lock().await.drain_requests(), Vec::new());
    assert_eq!(h.logs.drops().requests, 0);
}

/// A WebSocket is one row, written when the connection closes: status 101, and its total is
/// the connection's lifetime.
#[tokio::test]
async fn a_websocket_is_one_row_when_it_closes() {
    let h = Harness::start().await;
    let mut client = h.connect("app.example.com").await;
    client
        .write(
            b"GET /api/ws HTTP/1.1\r\nHost: app.example.com\r\nUpgrade: websocket\r\n\
              Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await;
    let (status, _) = client.head().await;
    assert_eq!(status, 101);
    let held = Duration::from_millis(300);
    tokio::time::sleep(held).await;
    assert_eq!(
        h.tap.lock().await.drain_requests(),
        Vec::new(),
        "still open"
    );
    drop(client);

    let row = h.next_request().await;
    assert_eq!(row.status, 101);
    assert_eq!(row.path, "/api/ws");
    assert!(row.total >= held, "{row:?}");
    assert!(row.to_header.is_some_and(|header| header < held), "{row:?}");
}

/// The proxy never waits for the store: with the recorder's channel full, requests are
/// answered as ever and their rows are dropped and counted.
#[tokio::test]
async fn a_full_log_channel_drops_rows_without_holding_requests() {
    let h = Harness::start_logging(
        Timeouts::default(),
        DownstreamLimits::default(),
        Admission::default(),
        1,
    )
    .await;
    for n in 0..3 {
        let started = Instant::now();
        assert_eq!(
            h.get("app.example.com", &format!("/full/{n}")).await.status,
            200
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    Drained::until(&h.traffic, |d| {
        d.routes.values().map(|r| r.statuses.total()).sum::<u64>() >= 3
    })
    .await;
    assert_eq!(h.logs.drops().requests, 2);
    let rows = h.tap.lock().await.drain_requests();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, "/full/0");
}

mod limits;
mod smuggling;

/// What scanners send matches no route, and is not in the log at `info`: an unknown host, a
/// host that is not the SNI name, and everything on `:80` but an answered ACME challenge.
#[tokio::test]
async fn requests_that_matched_no_route_are_not_logged_at_info() {
    let log = crate::logging::capture::global();
    let h = Harness::start().await;

    let reply = h.get("noroute.example.com", "/quiet/unknown-host").await;
    assert_eq!(reply.status, 404);
    let head =
        "GET /quiet/misdirected HTTP/1.1\r\nHost: api.example.com\r\nConnection: close\r\n\r\n";
    let reply = h.request_with_sni("app.example.com", head).await;
    assert_eq!(reply.status, 421);
    // They are still counted, and counting happens after the line would have been written.
    let drained = Drained::until(&h.traffic, |drained| {
        drained
            .routes
            .get("")
            .is_some_and(|unrouted| unrouted.statuses.total() == 2)
    })
    .await;
    assert_eq!(drained.route("").statuses.total(), 2, "{drained:?}");

    // On `:80`, one connection: its requests are logged in order, so the challenge's line
    // comes after whatever the probes before it wrote.
    h.challenges.insert("quiet-token", "quiet-token.thumbprint");
    let mut plain = h.connect_http().await;
    for (host, target, status) in [
        ("app.example.com", "/quiet/redirect", 308),
        ("203.0.113.9", "/quiet/probe", 404),
        (
            "app.example.com",
            "/.well-known/acme-challenge/unknown",
            404,
        ),
        (
            "app.example.com",
            "/.well-known/acme-challenge/quiet-token",
            200,
        ),
    ] {
        plain
            .write(format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
            .await;
        assert_eq!(plain.reply().await.status, status, "{target}");
    }
    let line = access_line("/.well-known/acme-challenge/quiet-token").await;
    assert_eq!(line["target"], "hoster::access");
    assert_eq!(line["level"], "DEBUG");
    assert_eq!(line["status"], 200);

    let noise: Vec<Value> = log
        .json()
        .into_iter()
        .filter(|line| {
            let path = line["path"].as_str().unwrap_or_default();
            path.starts_with("/quiet/") || path.ends_with("/unknown")
        })
        .collect();
    assert_eq!(noise, Vec::<Value>::new());
}
