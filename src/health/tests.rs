//! The checker against local test servers and [`FakeRuntime`].

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tokio::net::{TcpListener, TcpSocket};

use super::*;
use crate::runtime::{ContainerSpec, FakeRuntime, NetworkSpec, ServiceGeneration};
use crate::stack::{Name, Stack};
use crate::store::{DeploymentSource, DigestSet, NewDeployment};

/// Collects what the checker records.
#[derive(Default)]
struct Events(Mutex<Vec<String>>);

impl Events {
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl ProbeLog for Events {
    async fn record(&self, message: &str) {
        self.0.lock().unwrap().push(message.to_owned());
    }
}

/// How a test server answers the n-th request (0-based).
#[derive(Clone, Copy)]
enum Behaviour {
    /// 503 for the first `n` requests, 200 afterwards.
    HealthyAfter(usize),
    /// Always this status.
    Always(u16),
    /// 200 for even requests, 503 for odd ones.
    Alternating,
    /// Closes the connection without answering.
    Hangup,
    /// Accepts and never answers.
    Silent,
}

/// A local HTTP server; records the request lines it receives.
struct TestServer {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    crashed: Arc<AtomicBool>,
}

impl TestServer {
    async fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let crashed = Arc::new(AtomicBool::new(false));
        let down = Arc::clone(&crashed);
        let count = Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                if down.load(Ordering::SeqCst) {
                    continue;
                }
                let n = count.fetch_add(1, Ordering::SeqCst);
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 512];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let text = String::from_utf8_lossy(&head);
                    seen.lock()
                        .unwrap()
                        .push(text.lines().next().unwrap_or_default().to_owned());
                    let status = match behaviour {
                        Behaviour::HealthyAfter(k) if n < k => 503,
                        Behaviour::HealthyAfter(_) => 200,
                        Behaviour::Always(status) => status,
                        Behaviour::Alternating if n.is_multiple_of(2) => 200,
                        Behaviour::Alternating => 503,
                        Behaviour::Hangup => return,
                        Behaviour::Silent => {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                            return;
                        }
                    };
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                });
            }
        });
        Self {
            port,
            requests,
            crashed,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// The app crashes: like rootless podman's port forwarder, which keeps the loopback port
    /// open, the server still accepts connections but drops each one unanswered. (Closing the
    /// listener instead would free the port for a parallel test to take.)
    fn crash(&self) {
        self.crashed.store(true, Ordering::SeqCst);
    }
}

/// A loopback port held by a bound socket that never listens, so connections to it are refused
/// for as long as it lives. A port taken from a dropped listener would be free for a parallel
/// test's server to take, which then answers probes meant to fail.
struct ClosedPort {
    _socket: TcpSocket,
    port: u16,
}

impl ClosedPort {
    fn new() -> Self {
        // No SO_REUSEADDR: nobody else can bind the port while this socket holds it.
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let port = socket.local_addr().unwrap().port();
        Self {
            _socket: socket,
            port,
        }
    }
}

/// A stack with one service `app` routed to container port 8080, plus `health_toml`.
fn service(health_toml: &str) -> Service {
    let toml = format!(
        r#"
version = 1
name = "demo"

[services.app]
image = "docker.io/library/nginx@sha256:{digest}"

[[services.app.routes]]
host = "demo.example.com"
port = 8080

[[services.app.routes]]
host = "demo.example.com"
prefix = "/admin"
port = 9090
{health_toml}
"#,
        digest = "ab".repeat(32)
    );
    let stack: Stack = toml.parse().unwrap();
    stack.services[&"app".parse::<Name>().unwrap()].clone()
}

fn ports(container_port: u16, loopback: u16) -> PortMap {
    PortMap::from([(container_port, loopback), (9090, 1)])
}

/// A running container in a fresh [`FakeRuntime`].
async fn running() -> (FakeRuntime, ContainerId) {
    let runtime = FakeRuntime::new();
    let generation = ServiceGeneration {
        stack: "demo".parse().unwrap(),
        service: "app".parse().unwrap(),
        generation: 1,
    };
    let image = "docker.io/library/app:1"
        .parse::<crate::stack::ImageRef>()
        .unwrap()
        .pin(format!("sha256:{}", "cd".repeat(32)).parse().unwrap());
    let spec = ContainerSpec::new(&generation, DeploymentId::from(1), image.clone());
    runtime
        .ensure_network(&NetworkSpec::for_stack(&generation.stack))
        .await
        .unwrap();
    runtime.pull_image(&image).await.unwrap();
    let id = runtime.create_container(&spec).await.unwrap();
    runtime.start(&id).await.unwrap();
    (runtime, id)
}

const FAST: &str = r#"
[services.app.health]
http = { path = "/ready" }
interval = "10ms"
timeout = "200ms"
healthy_after = 2
start_period = "2s"
"#;

#[tokio::test]
async fn http_probe_passes_after_the_server_becomes_healthy() {
    let server = TestServer::start(Behaviour::HealthyAfter(3)).await;
    let check = Check::for_service(&service(FAST), &ports(8080, server.port)).unwrap();
    let (runtime, id) = running().await;
    let events = Events::default();
    check
        .run(&runtime, &id, &NetworkProber, &events)
        .await
        .unwrap();

    // Three 503s, then two passes in a row.
    let requests = server.requests();
    assert_eq!(requests.len(), 5, "{requests:?}");
    assert!(requests.iter().all(|r| r == "GET /ready HTTP/1.1"));
    let events = events.all();
    let addr = format!("127.0.0.1:{}", server.port);
    assert_eq!(
        events[0],
        format!(
            "health probe GET /ready on container port 8080 ({addr}): HTTP 503, expected 2xx-3xx"
        )
    );
    assert_eq!(
        events[1],
        format!("health probe GET /ready on container port 8080 ({addr}): HTTP 200")
    );
    assert!(
        events[2].starts_with(&format!(
            "health check GET /ready on container port 8080 ({addr}) passed 2 time(s) in a row"
        )),
        "{events:?}"
    );
    assert_eq!(
        events.len(),
        3,
        "a steady result is logged once: {events:?}"
    );
}

#[tokio::test]
async fn a_failure_resets_the_streak() {
    // Alternating 200/503 never gives two passes in a row.
    let server = TestServer::start(Behaviour::Alternating).await;
    let check = Check::for_service(&service(FAST), &ports(8080, server.port))
        .unwrap()
        .with_start_period(Duration::from_millis(150));
    let (runtime, id) = running().await;
    let err = check
        .run(&runtime, &id, &NetworkProber, &())
        .await
        .unwrap_err();
    assert!(matches!(err, HealthError::Unhealthy { .. }), "{err:?}");
    assert!(server.requests().len() >= 3);
}

#[tokio::test]
async fn an_exact_expected_status_rejects_other_codes() {
    let toml = FAST.replace(
        "http = { path = \"/ready\" }",
        "http = { path = \"/ready\", expect_status = \"204\" }",
    );
    let server = TestServer::start(Behaviour::Always(200)).await;
    let check = Check::for_service(&service(&toml), &ports(8080, server.port))
        .unwrap()
        .with_start_period(Duration::from_millis(100));
    let (runtime, id) = running().await;
    let err = check
        .run(&runtime, &id, &NetworkProber, &())
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .ends_with("last result: HTTP 200, expected 204"),
        "{err}"
    );
}

#[tokio::test]
async fn a_server_that_never_becomes_healthy_times_out() {
    let server = TestServer::start(Behaviour::Always(500)).await;
    let check = Check::for_service(&service(FAST), &ports(8080, server.port))
        .unwrap()
        .with_start_period(Duration::from_millis(150));
    let (runtime, id) = running().await;
    let events = Events::default();
    let started = Instant::now();
    let err = check
        .run(&runtime, &id, &NetworkProber, &events)
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(err, HealthError::Unhealthy { .. }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("did not pass within 150ms"), "{text}");
    assert!(text.contains("HTTP 500"), "{text}");
    assert!(!text.contains("health.http.path"), "explicit path: {text}");
    // The probe failure once, then the verdict.
    let events = events.all();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1], text);
}

#[tokio::test]
async fn a_server_that_hangs_up_or_stays_silent_fails() {
    let (runtime, id) = running().await;
    let hangup = TestServer::start(Behaviour::Hangup).await;
    let check = Check::for_service(&service(FAST), &ports(8080, hangup.port))
        .unwrap()
        .with_start_period(Duration::from_millis(100));
    let err = check
        .run(&runtime, &id, &NetworkProber, &())
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("connection closed before an HTTP status line"),
        "{err}"
    );

    let silent = TestServer::start(Behaviour::Silent).await;
    let toml = FAST.replace("timeout = \"200ms\"", "timeout = \"30ms\"");
    let check = Check::for_service(&service(&toml), &ports(8080, silent.port))
        .unwrap()
        .with_start_period(Duration::from_millis(100));
    let err = check
        .run(&runtime, &id, &NetworkProber, &())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no answer within 30ms"), "{err}");
}

#[tokio::test]
async fn a_crashing_server_is_unhealthy() {
    let server = TestServer::start(Behaviour::HealthyAfter(usize::MAX)).await;
    let check = Check::for_service(&service(FAST), &ports(8080, server.port))
        .unwrap()
        .with_start_period(Duration::from_millis(300));
    let (runtime, id) = running().await;
    let events = Events::default();
    let crash = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.crash();
    };
    let (result, ()) = tokio::join!(check.run(&runtime, &id, &NetworkProber, &events), crash);
    let dropped = |text: &str| {
        [
            "connection closed before an HTTP status line",
            "cannot read",
            "cannot send",
        ]
        .iter()
        .any(|why| text.contains(why))
    };
    let err = result.unwrap_err();
    assert!(dropped(&err.to_string()), "{err}");
    let events = events.all();
    assert!(
        events[0].ends_with("HTTP 503, expected 2xx-3xx"),
        "{events:?}"
    );
    assert!(dropped(&events[1]), "{events:?}");
}

#[tokio::test]
async fn a_container_exiting_during_the_check_aborts_early() {
    let server = TestServer::start(Behaviour::Always(503)).await;
    let check = Check::for_service(&service(FAST), &ports(8080, server.port))
        .unwrap()
        .with_start_period(Duration::from_secs(30));
    let (runtime, id) = running().await;
    let events = Events::default();
    let crash = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        runtime.set_state(&id, ContainerState::Exited(Some(137)));
    };
    let started = Instant::now();
    let (result, ()) = tokio::join!(check.run(&runtime, &id, &NetworkProber, &events), crash);
    assert!(started.elapsed() < Duration::from_secs(5), "aborted early");
    let err = result.unwrap_err();
    assert!(matches!(err, HealthError::Exited { .. }), "{err:?}");
    assert!(err.to_string().contains("exited"), "{err}");
    assert_eq!(events.all().last().unwrap(), &err.to_string());
}

#[tokio::test]
async fn routed_services_default_to_get_healthz_on_the_first_route_port() {
    let server = TestServer::start(Behaviour::Always(302)).await;
    let service = service("");
    assert_eq!(service.health, None);
    let check = Check::for_service(&service, &ports(8080, server.port)).unwrap();
    assert_eq!(
        check.to_string(),
        format!(
            "GET /healthz on container port 8080 (127.0.0.1:{})",
            server.port
        )
    );
    assert_eq!(check.interval, DEFAULT_INTERVAL);
    assert_eq!(check.timeout, DEFAULT_TIMEOUT);
    assert_eq!(check.healthy_after, DEFAULT_HEALTHY_AFTER);
    assert_eq!(check.start_period, DEFAULT_START_PERIOD);
    let (runtime, id) = running().await;
    // 3xx counts as healthy by default.
    check.run(&runtime, &id, &NetworkProber, &()).await.unwrap();
    assert_eq!(server.requests(), ["GET /healthz HTTP/1.1"]);
}

#[tokio::test]
async fn a_failing_default_probe_says_how_to_override_the_path() {
    let closed = ClosedPort::new();
    let check = Check::for_service(&service(""), &ports(8080, closed.port))
        .unwrap()
        .with_interval(Duration::from_millis(10))
        .with_start_period(Duration::from_millis(50));
    let (runtime, id) = running().await;
    let events = Events::default();
    let err = check
        .run(&runtime, &id, &NetworkProber, &events)
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("GET /healthz on container port 8080"),
        "{text}"
    );
    assert!(text.contains("set `health.http.path`"), "{text}");
    assert_eq!(events.all().last().unwrap(), &text);
}

#[tokio::test]
async fn tcp_probe_connects() {
    let toml = "[services.app.health]\ntcp = { port = 9090 }\ninterval = \"10ms\"\nstart_period = \"100ms\"";
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let check =
        Check::for_service(&service(toml), &PortMap::from([(8080, 1), (9090, port)])).unwrap();
    assert_eq!(
        check.to_string(),
        format!("TCP connect to container port 9090 (127.0.0.1:{port})")
    );
    let (runtime, id) = running().await;
    check.run(&runtime, &id, &NetworkProber, &()).await.unwrap();

    let closed = ClosedPort::new();
    let check = Check::for_service(
        &service(toml),
        &PortMap::from([(8080, 1), (9090, closed.port)]),
    )
    .unwrap();
    let err = check
        .run(&runtime, &id, &NetworkProber, &())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cannot connect"), "{err}");
}

#[test]
fn status_lines_need_a_three_digit_code() {
    assert_eq!(Check::parse_status_line("HTTP/1.1 200 OK"), Some(200));
    assert_eq!(Check::parse_status_line("HTTP/1.0 302"), Some(302));
    assert_eq!(
        Check::parse_status_line("HTTP/1.1 204 No Content"),
        Some(204)
    );
    for bad in [
        "",
        "HTTP/1.1",
        "HTTP/1.1 +200 OK",
        "HTTP/1.1 2000 OK",
        "HTTP/1.1 20 OK",
        "HTTP/2 200",
        "HTTP/1.11 200",
        "SSH-2.0-OpenSSH_9.8",
    ] {
        assert_eq!(Check::parse_status_line(bad), None, "{bad:?}");
    }
}

#[tokio::test]
async fn a_bare_lf_ends_the_status_line() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 512];
        let _ = stream.read(&mut buf).await;
        stream.write_all(b"HTTP/1.0 204\nX: y\n\n").await.unwrap();
    });
    assert_eq!(Check::http_status(addr, "/").await, Ok(204));
    server.await.unwrap();
}

#[test]
fn a_probe_port_without_a_loopback_port_is_an_error() {
    let err = Check::for_service(&service(""), &PortMap::new()).unwrap_err();
    assert!(matches!(err, HealthError::NoLoopbackPort(8080)), "{err:?}");
}

/// `health.exec` parses (ADR-0056) before hoster can run it; until then it fails clearly.
#[test]
fn exec_checks_are_not_run_yet() {
    let toml = format!(
        "version = 1\nname = \"demo\"\n[services.db]\nimage = \"docker.io/library/w@sha256:{}\"\nhealth = {{ exec = [\"pg_isready\"] }}",
        "ab".repeat(32)
    );
    let stack: Stack = toml.parse().unwrap();
    let db = &stack.services[&"db".parse::<Name>().unwrap()];
    let err = Check::for_service(db, &PortMap::new()).unwrap_err();
    assert!(matches!(err, HealthError::ExecUnsupported), "{err:?}");
}

#[tokio::test]
async fn services_without_routes_settle() {
    let toml = format!(
        "version = 1\nname = \"demo\"\n[services.worker]\nimage = \"docker.io/library/w@sha256:{}\"",
        "ab".repeat(32)
    );
    let stack: Stack = toml.parse().unwrap();
    let worker = &stack.services[&"worker".parse::<Name>().unwrap()];
    let check = Check::for_service(worker, &PortMap::new()).unwrap();
    assert_eq!(check, Check::settle(DEFAULT_SETTLE, DEFAULT_SETTLE_POLL));
    assert_eq!(check.to_string(), "running for 5s");

    let check = check
        .with_start_period(Duration::from_millis(40))
        .with_interval(Duration::from_millis(5));
    let (runtime, id) = running().await;
    check.run(&runtime, &id, &NetworkProber, &()).await.unwrap();

    runtime.set_state(&id, ContainerState::Exited(Some(1)));
    let err = check
        .run(&runtime, &id, &NetworkProber, &())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "container hoster-demo-app-g1 exited (1) before it became healthy"
    );
}

#[tokio::test]
async fn deployment_log_records_probe_results_as_events() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    store.add_revision("demo", "x").await.unwrap();
    let deployment = store
        .create_deployment(&NewDeployment {
            stack: "demo".into(),
            revision: None,
            source: DeploymentSource::Webhook,
            target: DigestSet::new(),
            provenance: None,
        })
        .await
        .unwrap();
    let closed = ClosedPort::new();
    let check = Check::for_service(&service(""), &ports(8080, closed.port))
        .unwrap()
        .with_interval(Duration::from_millis(10))
        .with_start_period(Duration::from_millis(30));
    let (runtime, id) = running().await;
    let log = DeploymentLog::new(
        store.clone(),
        deployment.id,
        DeploymentState::HealthChecking,
    );
    check
        .run(&runtime, &id, &NetworkProber, &log)
        .await
        .unwrap_err();

    let events = store.events(deployment.id).await.unwrap();
    let notes: Vec<_> = events
        .iter()
        .filter(|e| e.from == Some(DeploymentState::HealthChecking))
        .collect();
    assert_eq!(notes.len(), 2, "{events:?}");
    assert!(
        notes
            .iter()
            .all(|e| e.to == DeploymentState::HealthChecking)
    );
    assert!(
        notes[1]
            .message
            .as_deref()
            .unwrap()
            .contains("set `health.http.path`")
    );
    // A note leaves the deployment's state alone.
    let deployment = store.deployment(deployment.id).await.unwrap().unwrap();
    assert_eq!(deployment.state, DeploymentState::Received);
}
