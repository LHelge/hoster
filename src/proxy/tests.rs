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

use super::{Proxy, ProxyError, ProxyState, ProxyThread};
use crate::acme::{CertMap, ChallengeMap};
use crate::config::{ProxyConfig, Timeouts};
use crate::routing::{
    DesiredStack, DesiredState, DrainOutcome, Reserved, RouteTable, Target, UnixUpstream, Upstream,
};
use crate::stack::{
    DEFAULT_DRAIN_TIMEOUT, DEFAULT_STOP_TIMEOUT, Name, Route, RouteTimeouts, Service, Stack,
    UpdateStrategy,
};
use crate::stats::{StatusClass, Traffic};
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
    while let Some(chunk) = body.next().await {
        total += chunk.expect("upload body").len();
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
        track: None,
        command: None,
        entrypoint: None,
        env: BTreeMap::new(),
        secrets: BTreeMap::new(),
        volumes: BTreeMap::new(),
        health: None,
        update: UpdateStrategy::default(),
        stop_timeout: DEFAULT_STOP_TIMEOUT,
        user: None,
        read_only: false,
        routes,
    }
}

/// A proxy in front of one upstream app, with these routes:
///
/// - `app.example.com` `/` → web; `/api` (stripped) → web; `/keep` → web
/// - `api.example.com` `/v1` → web
/// - `slow.example.com` `/` → web with a 300 ms read timeout
/// - `down.example.com` `/` → a port nothing listens on
/// - `reset.example.com` `/` → a listener that drops every connection, counting them
/// - `flaky.example.com` `/` → a keep-alive listener that answers only the first request on
///   each connection and closes on the second, counting requests
/// - `gap.example.com` `/` → a service in its stop-first gap
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
    desired: DesiredState,
    reserved: Reserved,
    state: AppState,
    resets: Arc<AtomicUsize>,
    flaky_requests: Arc<AtomicUsize>,
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
        let services = BTreeMap::from([
            (
                name("web"),
                service(vec![
                    route("app.example.com", "/", false, none()),
                    route("app.example.com", "/api", true, none()),
                    route("app.example.com", "/keep", false, none()),
                    route("api.example.com", "/v1", false, none()),
                    route("slow.example.com", "/", false, short_read),
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
            "down.example.com",
            "reset.example.com",
            "flaky.example.com",
            "gap.example.com",
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
        let proxy_state = ProxyState {
            routes: Arc::clone(&routes),
            certs: certs.clone(),
            challenges: challenges.clone(),
            traffic: traffic.clone(),
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
            match Proxy::new(&config, proxy_state.clone()).spawn(rx.clone()) {
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
            desired,
            reserved,
            state,
            resets,
            flaky_requests,
            shutdown,
            thread: Some(thread),
            admin,
            _admin_server: admin_server,
            _socket_dir: socket_dir,
        }
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
    let deadline = Instant::now() + Duration::from_secs(5);
    let snapshot = loop {
        let snapshot = h.traffic.snapshot();
        let finished: u64 = snapshot
            .routes
            .iter()
            .map(|r| r.counts.statuses.total())
            .sum::<u64>()
            + snapshot.unrouted.statuses.total();
        if finished >= 5 || Instant::now() > deadline {
            break snapshot;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let route = |host: &str, prefix: &str| {
        snapshot
            .routes
            .iter()
            .find(|r| r.host == host && r.prefix == prefix)
            .unwrap_or_else(|| panic!("{host}{prefix} in {snapshot:?}"))
            .counts
    };
    let root = route("app.example.com", "/");
    assert_eq!(root.statuses.get(StatusClass::Success), 2);
    assert_eq!(root.latency.samples, 2);
    assert!(root.latency.p95.is_some());
    assert_eq!(root.inflight, 0);
    assert_eq!(route("app.example.com", "/api").statuses.total(), 1);
    let down = route("down.example.com", "/");
    assert_eq!(down.statuses.get(StatusClass::ServerError), 1);
    assert_eq!(down.upstream_errors, 1);
    assert_eq!(snapshot.unrouted.statuses.get(StatusClass::ClientError), 1);
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

/// 1 GiB up and 1 GiB down with bounded memory; `HOSTER_PROXY_STREAM_BYTES` overrides the size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_gib_streams_with_bounded_memory() {
    let bytes = std::env::var("HOSTER_PROXY_STREAM_BYTES")
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
async fn every_request_gets_an_access_log_line() {
    crate::logging::capture::global();
    let h = Harness::start().await;

    let reply = h.get("app.example.com", "/access/proxied?secret=1").await;
    assert_eq!(reply.status, 200);
    let line = access_line("/access/proxied").await;
    assert_eq!(line["target"], "hoster::access");
    assert_eq!(line["level"], "INFO");
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

    let reply = h.get("noroute.example.com", "/access/unknown").await;
    assert_eq!(reply.status, 404);
    let line = access_line("/access/unknown").await;
    assert_eq!(line["host"], "noroute.example.com");
    assert_eq!(line["status"], 404);
    assert!(line.get("route").is_none(), "{line}");
    assert!(line.get("upstream").is_none(), "{line}");

    let head =
        "GET /access/misdirected HTTP/1.1\r\nHost: api.example.com\r\nConnection: close\r\n\r\n";
    let reply = h.request_with_sni("app.example.com", head).await;
    assert_eq!(reply.status, 421);
    let line = access_line("/access/misdirected").await;
    assert_eq!(line["host"], "api.example.com");
    assert_eq!(line["status"], 421);
    assert!(line.get("route").is_none(), "{line}");

    // The `:80` service logs its redirects too.
    let reply = h.get_http("app.example.com", "/access/plain?token=1").await;
    assert_eq!(reply.status, 308);
    let line = access_line("/access/plain").await;
    assert_eq!(line["host"], "app.example.com");
    assert_eq!(line["status"], 308);
    assert_eq!(line["client"], "127.0.0.1");
    assert!(line.get("route").is_none(), "{line}");
    assert!(!line.to_string().contains("token"), "{line}");

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
