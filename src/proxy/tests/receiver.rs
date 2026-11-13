//! The OTLP receiver's route on the real proxy's `:80` (ADR-0069 §3), in front of the real
//! receiver served on its Unix socket the way the supervisor serves it.
//!
//! A client cannot connect from an address that is not one of the host's own, so the tests
//! stand in for the host's addresses with [`Listed`]: connections come from addresses in
//! `127/8` (bound before connecting, as the connection-cap tests do), and only the listed ones
//! count as local. A peer that is the connection's own local address counts as local whatever
//! the list says, which is how the proxy's own address `127.0.0.1` passes.

use std::net::{IpAddr, Ipv4Addr};

use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, ScopeSpans, Span};
use prost::Message as _;

use super::*;
use crate::otlp::{OtlpLimits, OtlpReceiver};
use crate::proxy::HostAddresses;
use crate::secrets::{KnownServices, SecretKey, TokenKey};

const PROXY: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const LISTED: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));
const OUTSIDER: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3));

/// The host's own addresses, as far as the proxy under test knows: [`LISTED`] only.
#[derive(Debug)]
struct Listed;

impl HostAddresses for Listed {
    fn contains(&self, ip: IpAddr) -> bool {
        ip == LISTED
    }
}

/// `shop/web` exists, nothing else.
struct Shop;

impl KnownServices for Shop {
    fn contains(&self, stack: &Name, service: &Name) -> bool {
        stack.as_ref() == "shop" && service.as_ref() == "web"
    }
}

/// A proxy whose `:80` hands the receiver's host to a real receiver.
struct Receiving {
    http: SocketAddr,
    https: SocketAddr,
    ca: TestCa,
    key: TokenKey,
    tap: Tap,
    traffic: Traffic,
    admission: Admission,
    shutdown: watch::Sender<Option<ShutdownSignal>>,
    receiver: WebServer,
    _dir: tempfile::TempDir,
}

impl Receiving {
    /// The proxy with `:80` on `http_ip` (a dual-stack listener for `::`), taking [`Listed`]
    /// for the host's addresses.
    async fn start(http_ip: IpAddr) -> Self {
        Self::start_with(http_ip, Arc::new(Listed)).await
    }

    /// The proxy with `:80` on `http_ip`, taking `addresses` for the host's addresses.
    async fn start_with(http_ip: IpAddr, addresses: Arc<dyn HostAddresses>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let key = TokenKey::from(&SecretKey::generate());
        let (logs, tap) = LogSink::tapped(64);
        let socket = dir.path().join(crate::config::Config::OTLP_SOCKET_NAME);
        let receiver = OtlpReceiver::new(
            key.clone(),
            Arc::new(Shop),
            logs.clone(),
            OtlpLimits::default(),
        );
        let server = WebServer::unix(
            receiver.router(),
            &socket,
            "OTLP receiver",
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

        // A known host, redirected on `:80` and served on `:443`.
        let reserved = Reserved::default().with_unix_host(
            "app.example.com".parse().unwrap(),
            "test",
            Arc::new(UnixUpstream::from(dir.path().join("app.sock"))),
        );
        let table = RouteTable::build(&DesiredState::default(), &reserved).unwrap();
        let ca = TestCa::new();
        let (certs, _) = CertMap::open(dir.path(), None).unwrap();
        let (chain, cert_key) = ca.issue("app.example.com");
        certs.insert("app.example.com", chain, cert_key).unwrap();
        let traffic = Traffic::default();
        let admission = Admission::default();
        let state = ProxyState {
            routes: Arc::new(ArcSwap::from_pointee(table)),
            certs,
            challenges: ChallengeMap::default(),
            traffic: traffic.clone(),
            admission: admission.clone(),
            logs,
            receiver: ReceiverRoute::new(socket).with_host_addresses(addresses),
        };
        let (shutdown, rx) = watch::channel(None);
        let mut attempts = 0;
        let (http, https, mut events) = loop {
            let free = |ip: IpAddr| {
                std::net::TcpListener::bind(SocketAddr::new(ip, 0))
                    .unwrap()
                    .local_addr()
                    .unwrap()
            };
            let (http, https) = (free(http_ip), free(PROXY));
            let config = ProxyConfig {
                http_listen: http,
                https_listen: https,
                threads: NonZeroUsize::new(2).unwrap(),
                grace_period: Duration::from_secs(1),
                graceful_shutdown_timeout: Duration::from_secs(1),
                timeouts: Timeouts::default(),
            };
            attempts += 1;
            match Proxy::new(&config, state.clone()).spawn(rx.clone()) {
                Ok(running) => break (http, https, running.events),
                Err(ProxyError::Bind { .. }) if attempts < 5 => continue,
                Err(e) => panic!("{e}"),
            }
        };
        events.ready(Duration::from_secs(10)).await.unwrap();
        Self {
            http,
            https,
            ca,
            key,
            tap,
            traffic,
            admission,
            shutdown,
            receiver: server,
            _dir: dir,
        }
    }

    /// A plain connection to `:80` from `from`.
    async fn connect_from(&self, from: IpAddr) -> Client<TcpStream> {
        let socket = match from {
            IpAddr::V4(_) => tokio::net::TcpSocket::new_v4(),
            IpAddr::V6(_) => tokio::net::TcpSocket::new_v6(),
        }
        .unwrap();
        socket.bind(SocketAddr::new(from, 0)).unwrap();
        let to = SocketAddr::new(PROXY, self.http.port());
        Client(BufReader::new(socket.connect(to).await.unwrap()))
    }

    /// `POST /v1/traces` with one span to `:80` from `from`, as `shop/web` when `authorized`.
    async fn export_from(&self, from: IpAddr, host: &str, authorized: bool) -> Reply {
        let mut client = self.connect_from(from).await;
        client.write(&self.export(host, authorized, true)).await;
        client.reply().await
    }

    /// The bytes of an export request; `close` asks for `Connection: close`.
    fn export(&self, host: &str, authorized: bool, close: bool) -> Vec<u8> {
        let body = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                scope_spans: vec![ScopeSpans {
                    spans: vec![Span {
                        trace_id: vec![0xab; 16],
                        span_id: vec![0x01; 8],
                        name: "GET /".to_owned(),
                        ..Span::default()
                    }],
                    ..ScopeSpans::default()
                }],
                ..ResourceSpans::default()
            }],
        }
        .encode_to_vec();
        let mut head = format!(
            "POST /v1/traces HTTP/1.1\r\nHost: {host}\r\n\
             Content-Type: application/x-protobuf\r\nContent-Length: {}\r\n",
            body.len()
        );
        if authorized {
            let token = self
                .key
                .token(&"shop".parse().unwrap(), &"web".parse().unwrap());
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if close {
            head.push_str("Connection: close\r\n");
        }
        head.push_str("\r\n");
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&body);
        bytes
    }

    /// The spans the receiver offered so far, after a moment for any still on their way.
    async fn spans(&mut self) -> usize {
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.tap.drain_spans().len()
    }
}

impl Drop for Receiving {
    fn drop(&mut self) {
        self.shutdown
            .send_replace(Some(ShutdownSignal::FastShutdown));
    }
}

/// The receiver's own answers: OTLP's protobuf, not the proxy's text pages.
fn from_the_receiver(reply: &Reply) -> bool {
    reply.header("content-type") == Some("application/x-protobuf")
}

#[tokio::test]
async fn a_local_peer_reaches_the_receiver_on_port_80() {
    let mut h = Receiving::start(PROXY).await;
    // The proxy's own address: the peer is the connection's local address.
    let reply = h.export_from(PROXY, "host.containers.internal", true).await;
    assert_eq!(
        reply.status,
        200,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(from_the_receiver(&reply));
    assert_eq!(h.spans().await, 1);
    // Another of the host's addresses, and the host as SDKs may spell it.
    let reply = h
        .export_from(LISTED, "HOST.containers.internal:80", true)
        .await;
    assert_eq!(reply.status, 200);
    assert_eq!(h.spans().await, 1);
    // The token still decides: the peer check is not identity.
    let reply = h
        .export_from(PROXY, "host.containers.internal", false)
        .await;
    assert_eq!(reply.status, 401);
    assert!(from_the_receiver(&reply));
    assert_eq!(h.spans().await, 0);

    // No request rows and no traffic buckets for any of them (ADR-0069 Consequences).
    assert!(h.tap.drain_requests().is_empty());
    assert!(h.traffic.drain().is_empty());
}

#[tokio::test]
async fn any_other_peer_gets_the_unknown_hosts_404() {
    let mut h = Receiving::start(PROXY).await;
    let reply = h
        .export_from(OUTSIDER, "host.containers.internal", true)
        .await;
    assert_eq!(reply.status, 404);
    assert_eq!(reply.text(), "404 Not Found\n");
    assert!(!from_the_receiver(&reply));
    assert_eq!(h.spans().await, 0);
    // The same as for any host hoster does not know.
    let unknown = h.export_from(OUTSIDER, "nope.example.com", true).await;
    assert_eq!((unknown.status, unknown.body), (404, reply.body));
}

/// A dual-stack listener sees IPv4 peers as IPv4-mapped IPv6 addresses; they are unmapped
/// before the comparison, against the connection's own address and against the host's.
#[tokio::test]
async fn ipv4_mapped_peers_are_unmapped_before_the_check() {
    let mut h = Receiving::start(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)).await;
    for (from, status) in [(PROXY, 200), (LISTED, 200), (OUTSIDER, 404)] {
        let reply = h.export_from(from, "host.containers.internal", true).await;
        assert_eq!(reply.status, status, "{from}");
    }
    assert_eq!(h.spans().await, 2);
}

#[tokio::test]
async fn the_route_comes_before_the_redirect_and_is_never_served_on_443() {
    let mut h = Receiving::start(PROXY).await;
    // A known host on `:80` is redirected; the receiver's host never is.
    let mut client = h.connect_from(PROXY).await;
    client
        .write(b"GET / HTTP/1.1\r\nHost: app.example.com\r\nConnection: close\r\n\r\n")
        .await;
    assert_eq!(client.reply().await.status, 308);
    let mut client = h.connect_from(PROXY).await;
    client
        .write(b"GET /v1/metrics HTTP/1.1\r\nHost: host.containers.internal\r\nConnection: close\r\n\r\n")
        .await;
    let reply = client.reply().await;
    // The receiver's own 404 for metrics (ADR-0069 §2), not a redirect.
    assert_eq!(reply.status, 404);
    assert!(from_the_receiver(&reply));

    // `:443` has no certificate for the name…
    let connector = h.ca.connector(&[b"http/1.1"]);
    let tcp = TcpStream::connect(h.https).await.unwrap();
    let name = ServerName::try_from("host.containers.internal".to_owned()).unwrap();
    assert!(connector.connect(name, tcp).await.is_err());
    // …and under another name's certificate, the host is misdirected.
    let tcp = TcpStream::connect(h.https).await.unwrap();
    let name = ServerName::try_from("app.example.com".to_owned()).unwrap();
    let mut client = Client(BufReader::new(connector.connect(name, tcp).await.unwrap()));
    client
        .write(&h.export("host.containers.internal", true, true))
        .await;
    assert_eq!(client.reply().await.status, 421);
    assert_eq!(h.spans().await, 0);
}

/// Receiver connections count against the caps of ADR-0065 like any other from their address.
#[tokio::test]
async fn a_receiver_connection_holds_a_slot_of_its_address() {
    let h = Receiving::start(PROXY).await;
    let before = h.admission.open_from_ip(LISTED);
    let mut client = h.connect_from(LISTED).await;
    client
        .write(&h.export("host.containers.internal", true, false))
        .await;
    assert_eq!(client.reply().await.status, 200);
    assert_eq!(h.admission.open_from_ip(LISTED), before + 1);
    // Kept alive: a second export on the same connection.
    client
        .write(&h.export("host.containers.internal", true, false))
        .await;
    assert_eq!(client.reply().await.status, 200);
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.admission.open_from_ip(LISTED) != before && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.admission.open_from_ip(LISTED), before);
}

#[tokio::test]
async fn a_receiver_that_is_not_listening_is_a_502() {
    let mut h = Receiving::start(PROXY).await;
    h.receiver.stop(true).await.unwrap();
    let reply = h.export_from(PROXY, "host.containers.internal", true).await;
    assert_eq!(reply.status, 502);
    assert_eq!(reply.text(), "502 Bad Gateway\n");
}

/// From a real container on a stack network, through pasta, to the real proxy and receiver,
/// with the host's real addresses. The proxy cannot listen on port 80 in a test, so the
/// container names the port; the route ignores it. Needs rootless podman
/// (`TEST_HOSTER_PODMAN=1`); busybox is pulled from Docker Hub on first use.
#[tokio::test]
async fn a_container_reaches_the_receiver_through_the_proxy() {
    if !std::env::var("TEST_HOSTER_PODMAN").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to run against rootless podman");
        return;
    }
    let mut h = Receiving::start_with(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        Arc::new(crate::proxy::Interfaces),
    )
    .await;
    let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(std::path::PathBuf::from);
    let runtime = crate::runtime::PodmanRuntime::connect(&crate::runtime::PodmanSocket::from_env(
        socket.as_deref(),
    ))
    .await
    .expect("connect to rootless podman");
    let probe = crate::doctor::ReceiverPath::new(&runtime);
    let url = format!(
        "http://host.containers.internal:{}/v1/traces",
        h.http.port()
    );
    let token = h
        .key
        .token(&"shop".parse().unwrap(), &"web".parse().unwrap())
        .to_string();
    let seen = probe.probe_at(&url, Some(&token)).await.unwrap();
    assert_eq!(
        seen.address,
        Some("169.254.1.2".parse().unwrap()),
        "{seen:?}"
    );
    // An empty export: the receiver's 200, so the peer check let the container through.
    assert_eq!(seen.status, Some(200), "{seen:?}");
    let seen = probe.probe_at(&url, None).await.unwrap();
    assert_eq!(seen.status, Some(401), "{seen:?}");
    assert!(h.tap.drain_requests().is_empty());
    assert!(h.traffic.drain().is_empty());
    assert_eq!(h.spans().await, 0);
}
