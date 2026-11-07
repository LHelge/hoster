//! End-to-end tests of the routing proxy: a real Pingora server on loopback in front of
//! in-process axum upstreams, driven by a small raw HTTP/1.1 client so every byte on the wire is
//! under the test's control.

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
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};

use super::{Proxy, ProxyThread};
use crate::config::ProxyConfig;
use crate::routing::{DesiredStack, DesiredState, Reserved, RouteTable, UnixUpstream, Upstream};
use crate::stack::{
    DEFAULT_DRAIN_TIMEOUT, DEFAULT_STOP_TIMEOUT, Name, Route, RouteTimeouts, Service, Stack,
    UpdateStrategy,
};

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
struct Harness {
    proxy: SocketAddr,
    web: Arc<Upstream>,
    state: AppState,
    resets: Arc<AtomicUsize>,
    flaky_requests: Arc<AtomicUsize>,
    shutdown: watch::Sender<Option<ShutdownSignal>>,
    thread: Option<std::thread::JoinHandle<()>>,
    _socket_dir: tempfile::TempDir,
}

impl Harness {
    async fn start() -> Self {
        let state = AppState::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let web_port = listener.local_addr().unwrap().port();
        tokio::spawn(axum::serve(listener, upstream_app(state.clone())).into_future());

        let socket_dir = tempfile::tempdir().unwrap();
        let socket = socket_dir.path().join("admin.sock");
        let unix = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(axum::serve(unix, upstream_app(state.clone())).into_future());

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
        let reserved = Reserved::default().with_unix_host(
            "admin.example.com".parse().unwrap(),
            "admin UI",
            Arc::new(UnixUpstream::from(socket)),
        );
        let table = RouteTable::build(&desired, &reserved).unwrap();

        let proxy = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let config = ProxyConfig {
            http_listen: proxy,
            threads: NonZeroUsize::new(2).unwrap(),
            grace_period: Duration::from_secs(1),
            graceful_shutdown_timeout: Duration::from_secs(1),
            ..ProxyConfig::default()
        };
        let routes = Arc::new(ArcSwap::from_pointee(table));
        let (shutdown, rx) = watch::channel(None);
        let ProxyThread { thread, mut events } = Proxy::new(&config, routes).spawn(rx).unwrap();
        events.ready(Duration::from_secs(10)).await.unwrap();
        Harness {
            proxy,
            web,
            state,
            resets,
            flaky_requests,
            shutdown,
            thread: Some(thread),
            _socket_dir: socket_dir,
        }
    }

    async fn connect(&self) -> Client {
        Client(BufReader::new(
            TcpStream::connect(self.proxy).await.unwrap(),
        ))
    }

    /// One `GET` with `Connection: close`.
    async fn get(&self, host: &str, target: &str) -> Reply {
        self.request(&format!(
            "GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
        ))
        .await
    }

    async fn request(&self, head: &str) -> Reply {
        let mut client = self.connect().await;
        client.write(head.as_bytes()).await;
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

struct Client(BufReader<TcpStream>);

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

impl Client {
    async fn write(&mut self, bytes: &[u8]) {
        self.0.get_mut().write_all(bytes).await.unwrap();
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

    let reply = h.get("unknown.example.com", "/").await;
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
async fn reaches_a_unix_socket_target() {
    let h = Harness::start().await;
    let reply = h.get("admin.example.com", "/healthz").await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["path"], "/healthz");
    assert_eq!(echoed(&reply, "host"), ["admin.example.com"]);
}

#[tokio::test]
async fn connect_failure_is_a_502_page() {
    let h = Harness::start().await;
    let reply = h.get("down.example.com", "/").await;
    assert_eq!(reply.status, 502);
    assert_eq!(reply.text(), "502 Bad Gateway\n");
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
    let mut client = h.connect().await;
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

    let mut client = h.connect().await;
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
    // This listener is plain HTTP; behind TLS it says https.
    assert_eq!(echoed(&reply, "x-forwarded-proto"), ["http"]);
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
    let mut client = h.connect().await;
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

#[tokio::test]
async fn websockets_pass_through() {
    let h = Harness::start().await;
    let mut client = h.connect().await;
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

    let mut client = h.connect().await;
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

    let mut client = h.connect().await;
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
