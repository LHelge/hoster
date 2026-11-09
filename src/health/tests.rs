//! The checker against local test servers and [`FakeRuntime`].

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tokio::net::{TcpListener, TcpSocket};

use super::*;
use crate::runtime::{
    ContainerSpec, ExecGuard, FakeRuntime, NetworkSpec, Operation, RunningExec, ServiceGeneration,
};
use crate::secrets::SecretValue;
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

/// Probes through `runtime` and the network, with exec slots of their own.
fn net(runtime: &FakeRuntime) -> Probes<'_, FakeRuntime> {
    Probes::new(runtime, &NetworkProber, &ExecSlots::default())
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
    check.run(&net(&runtime), &id, &events).await.unwrap();

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
    let err = check.run(&net(&runtime), &id, &()).await.unwrap_err();
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
    let err = check.run(&net(&runtime), &id, &()).await.unwrap_err();
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
    let err = check.run(&net(&runtime), &id, &events).await.unwrap_err();
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
    let err = check.run(&net(&runtime), &id, &()).await.unwrap_err();
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
    let err = check.run(&net(&runtime), &id, &()).await.unwrap_err();
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
    let probes = net(&runtime);
    let (result, ()) = tokio::join!(check.run(&probes, &id, &events), crash);
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
    let probes = net(&runtime);
    let (result, ()) = tokio::join!(check.run(&probes, &id, &events), crash);
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
    check.run(&net(&runtime), &id, &()).await.unwrap();
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
    let err = check.run(&net(&runtime), &id, &events).await.unwrap_err();
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
    check.run(&net(&runtime), &id, &()).await.unwrap();

    let closed = ClosedPort::new();
    let check = Check::for_service(
        &service(toml),
        &PortMap::from([(8080, 1), (9090, closed.port)]),
    )
    .unwrap();
    let err = check.run(&net(&runtime), &id, &()).await.unwrap_err();
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

/// A stack with one service `app` without routes, plus `health_toml`.
fn unrouted(health_toml: &str) -> Service {
    let toml = format!(
        "version = 1\nname = \"demo\"\n[services.app]\nimage = \"docker.io/library/w@sha256:{}\"\n{health_toml}",
        "ab".repeat(32)
    );
    let stack: Stack = toml.parse().unwrap();
    stack.services[&"app".parse::<Name>().unwrap()].clone()
}

const EXEC: &str = r#"
[services.app.health]
exec = ["pg_isready", "-U", "postgres"]
interval = "1s"
start_period = "10s"
"#;

fn exited(code: i64, output: &str) -> crate::runtime::fake::ExecResult {
    Ok(ExecOutcome::finished(code, output.into()))
}

/// An exec that outlives its timeout and keeps running until `guard` is dropped.
fn hung() -> (crate::runtime::fake::ExecResult, ExecGuard) {
    let (exec, guard) = RunningExec::new();
    let outcome = ExecOutcome::TimedOut {
        output: "still waiting\n".into(),
        exec,
    };
    (Ok(outcome), guard)
}

/// ADR-0056: declared `health` (exec on any service) → `GET /healthz` for routed services →
/// running for 5 s; an exec check on a routed service also connects to the first route's port.
#[test]
fn checks_follow_the_precedence_of_adr_0056() {
    let ports = ports(8080, 20001);
    let exec = "[services.app.health]\nexec = [\"pg_isready\", \"-U\", \"postgres\"]";
    let table = [
        (
            service(""),
            "GET /healthz on container port 8080 (127.0.0.1:20001), the default for a routed \
             service without health",
            DEFAULT_TIMEOUT,
        ),
        (
            service("[services.app.health]\nhttp = { path = \"/up\", port = 9090 }"),
            "GET /up on container port 9090 (127.0.0.1:1), declared in health.http",
            DEFAULT_TIMEOUT,
        ),
        (
            service("[services.app.health]\ntcp = { port = 9090 }"),
            "TCP connect to container port 9090 (127.0.0.1:1), declared in health.tcp",
            DEFAULT_TIMEOUT,
        ),
        (
            service(exec),
            "exec: pg_isready -U postgres and TCP connect to container port 8080 \
             (127.0.0.1:20001), declared in health.exec; a routed service's exec check also \
             connects to its first route's port",
            DEFAULT_EXEC_TIMEOUT,
        ),
        (
            unrouted(exec),
            "exec: pg_isready -U postgres, declared in health.exec",
            DEFAULT_EXEC_TIMEOUT,
        ),
        (
            unrouted(""),
            "running for 5s, the default for a service without routes or health",
            DEFAULT_TIMEOUT,
        ),
    ];
    for (service, explained, timeout) in table {
        let check = Check::for_service(&service, &ports).unwrap();
        assert_eq!(format!("{check:#}"), explained);
        assert!(explained.starts_with(&check.to_string()), "{check}");
        assert_eq!(check.timeout, timeout, "{check}");
        if !check.is_settle() {
            assert_eq!(check.interval, DEFAULT_INTERVAL, "{check}");
            assert_eq!(check.start_period, DEFAULT_START_PERIOD, "{check}");
        }
    }
    // A declared timeout wins over the exec default.
    let check =
        Check::for_service(&unrouted(&format!("{exec}\ntimeout = \"9s\"")), &ports).unwrap();
    assert_eq!(check.timeout, Duration::from_secs(9));
    // The exec check of a routed service needs the first route's loopback port.
    let err = Check::for_service(&service(exec), &PortMap::from([(9090, 1)])).unwrap_err();
    assert!(matches!(err, HealthError::NoLoopbackPort(8080)), "{err:?}");
}

#[tokio::test(start_paused = true)]
async fn an_exec_check_passes_once_the_command_exits_0() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new()).unwrap();
    let (runtime, id) = running().await;
    runtime.script_exec(
        "-app-",
        [
            exited(2, "/var/run/postgresql:5432 - no response\n"),
            exited(2, "/var/run/postgresql:5432 - no response\n"),
            exited(0, "/var/run/postgresql:5432 - accepting connections\n"),
        ],
    );
    let events = Events::default();
    let started = Instant::now();
    check.run(&net(&runtime), &id, &events).await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(2), "two intervals");

    let argv = ["pg_isready", "-U", "postgres"].map(str::to_owned).to_vec();
    assert_eq!(
        runtime.execs(),
        vec![("hoster-demo-app-g1".to_owned(), argv); 3]
    );
    let events = events.all();
    assert_eq!(
        events[0],
        "health probe exec: pg_isready -U postgres: exit 2, output \
         \"/var/run/postgresql:5432 - no response\"",
        "{events:?}"
    );
    assert!(
        events[1].starts_with("health check exec: pg_isready -U postgres passed 1 time(s)"),
        "{events:?}"
    );
    assert_eq!(events.len(), 2, "a steady failure is one event: {events:?}");
}

#[tokio::test(start_paused = true)]
async fn a_failing_exec_check_fails_at_the_start_period() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new()).unwrap();
    let (runtime, id) = running().await;
    runtime.script_exec("-app-", [exited(1, "no response from /etc/app.conf\n")]);
    let events = Events::default();
    let err = check.run(&net(&runtime), &id, &events).await.unwrap_err();
    assert!(matches!(err, HealthError::Unhealthy { .. }), "{err:?}");
    // The error is the deployment's failure reason (record, SSE, event log): no output in it.
    assert_eq!(
        err.to_string(),
        "health check exec: pg_isready -U postgres did not pass within 10s: last result: exit 1"
    );
    assert_eq!(
        runtime.execs().len(),
        11,
        "one per second, the last at the deadline"
    );
    // The output tail goes to the probe's deployment event alone.
    assert_eq!(
        events.all(),
        [
            "health probe exec: pg_isready -U postgres: exit 1, output \
             \"no response from /etc/app.conf\""
                .to_owned(),
            err.to_string()
        ]
    );

    // The monitor: the exit code only.
    let probes = net(&runtime);
    assert_eq!(check.live(&probes, &id).await, Err("exit 1".to_owned()));
    runtime.script_exec("-app-", [exited(0, "accepting connections\n")]);
    assert_eq!(check.live(&probes, &id).await, Ok("exit 0".to_owned()));
}

#[tokio::test(start_paused = true)]
async fn output_that_changes_between_probes_is_no_status_change() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new())
        .unwrap()
        .with_start_period(Duration::from_secs(5));
    let (runtime, id) = running().await;
    runtime.script_exec(
        "-app-",
        (0..6).map(|n| exited(2, &format!("attempt {n}: no response\n"))),
    );
    let events = Events::default();
    let err = check.run(&net(&runtime), &id, &events).await.unwrap_err();
    assert_eq!(runtime.execs().len(), 6);
    let events = events.all();
    assert_eq!(
        events,
        [
            "health probe exec: pg_isready -U postgres: exit 2, output \"attempt 0: no response\""
                .to_owned(),
            err.to_string()
        ],
        "a steady failure is one event"
    );
    assert!(err.to_string().ends_with("last result: exit 2"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_exec_is_a_failure_and_at_most_three_are_abandoned() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new())
        .unwrap()
        .with_start_period(Duration::from_secs(20));
    let (runtime, id) = running().await;
    let (a, guard_a) = hung();
    let (b, _guard_b) = hung();
    let (c, _guard_c) = hung();
    runtime.script_exec("-app-", [a, b, c, exited(0, "")]);
    let execs = ExecSlots::default();
    let probes = Probes::new(&runtime, &NetworkProber, &execs);
    let events = Events::default();
    let started = Instant::now();
    let err = check.run(&probes, &id, &events).await.unwrap_err();

    // Three probes time out after 5 s each (plus an interval between them); the fourth finds
    // three execs still running at 18 s and waits for one until the deadline.
    assert_eq!(runtime.execs().len(), 3, "no fourth exec while three hang");
    assert_eq!(execs.abandoned(&id), 3);
    assert_eq!(started.elapsed(), Duration::from_secs(20));
    let events = events.all();
    assert_eq!(
        events[0],
        "health probe exec: pg_isready -U postgres: no result within 5s, output \
         \"still waiting\"",
        "{events:?}"
    );
    assert!(
        err.to_string()
            .ends_with("last result: check stuck: 3 checks still running"),
        "{err}"
    );

    // The monitor starts none at the cap either, and says why at once.
    let before = Instant::now();
    assert_eq!(
        check.live(&probes, &id).await,
        Err("check stuck: 3 checks still running".to_owned())
    );
    assert_eq!(before.elapsed(), Duration::ZERO);

    // Once one ends, the next probe runs again.
    drop(guard_a);
    assert_eq!(check.live(&probes, &id).await, Ok("exit 0".to_owned()));
    assert_eq!(execs.abandoned(&id), 2);
}

#[tokio::test(start_paused = true)]
async fn a_deploy_probe_at_the_cap_waits_for_an_abandoned_exec_to_end() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new())
        .unwrap()
        .with_start_period(Duration::from_secs(30));
    let (runtime, id) = running().await;
    let (a, guard) = hung();
    let (b, _b) = hung();
    let (c, _c) = hung();
    runtime.script_exec("-app-", [a, b, c, exited(0, "")]);
    let execs = ExecSlots::default();
    let probes = Probes::new(&runtime, &NetworkProber, &execs);
    for _ in 0..3 {
        assert_eq!(
            check.live(&probes, &id).await,
            Err("no result within 5s".to_owned())
        );
    }
    assert_eq!(execs.abandoned(&id), 3);

    // One of them ends 12 s into the deploy probe, within its start period.
    let release = async {
        tokio::time::sleep(Duration::from_secs(12)).await;
        drop(guard);
    };
    let started = Instant::now();
    let (result, ()) = tokio::join!(check.run(&probes, &id, &()), release);
    result.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(12));
    assert_eq!(runtime.execs().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn exec_probes_of_one_container_never_overlap() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new()).unwrap();
    let (runtime, id) = running().await;
    let (a, _a) = hung();
    let (b, _b) = hung();
    runtime.script_exec("-app-", [a, b]);
    let probes = net(&runtime);
    let started = Instant::now();
    let (first, second) = tokio::join!(check.live(&probes, &id), check.live(&probes, &id));
    assert!(first.is_err() && second.is_err());
    assert_eq!(
        started.elapsed(),
        2 * DEFAULT_EXEC_TIMEOUT,
        "the second waited for the first"
    );
}

#[tokio::test(start_paused = true)]
async fn an_exec_check_that_cannot_run_aborts_at_once() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new()).unwrap();
    let (runtime, id) = running().await;
    let not_found = "Error: runc: exec failed: unable to start container process: exec: \
                     \"pg_isready\": executable file not found in $PATH: OCI runtime attempted \
                     to invoke a command that was not found\n";
    runtime.script_exec("-app-", [exited(127, not_found)]);
    let events = Events::default();
    let started = Instant::now();
    let err = check.run(&net(&runtime), &id, &events).await.unwrap_err();
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(matches!(err, HealthError::CannotRun(_)), "{err:?}");
    assert_eq!(
        err.to_string(),
        "`health.exec` cannot run: `pg_isready`: executable file not found in $PATH"
    );
    assert_eq!(events.all(), [err.to_string()]);
    // The monitor reports it as a failure with the reason.
    assert_eq!(check.live(&net(&runtime), &id).await, Err(err.to_string()));

    // A shell's 127 is an ordinary failure, retried.
    runtime.script_exec(
        "-app-",
        [exited(127, "sh: pg_isready: not found\n"), exited(0, "")],
    );
    check.run(&net(&runtime), &id, &()).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_container_stopping_during_an_exec_aborts_at_once() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new()).unwrap();
    let (runtime, id) = running().await;
    // Compat inspect still says running while podman refuses the exec.
    runtime.fail_next(
        Operation::Exec,
        RuntimeError::NotRunning {
            name: id.to_string(),
        },
    );
    let started = Instant::now();
    let err = check.run(&net(&runtime), &id, &()).await.unwrap_err();
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(matches!(err, HealthError::Stopping(_)), "{err:?}");

    // Exited by the time it is looked at: the exit is the reason.
    runtime.script_exec("-app-", [exited(1, "")]);
    let crash = async {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        runtime.set_state(&id, ContainerState::Exited(Some(137)));
    };
    let started = Instant::now();
    let probes = net(&runtime);
    let (result, ()) = tokio::join!(check.run(&probes, &id, &()), crash);
    let err = result.unwrap_err();
    assert!(matches!(err, HealthError::Exited { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(3), "aborted early");
    assert_eq!(
        check.live(&net(&runtime), &id).await,
        Err("the container is stopping or stopped".to_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn a_container_exiting_while_an_exec_runs_aborts_at_its_end() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new()).unwrap();
    let (runtime, id) = running().await;
    let (exec, _guard) = hung();
    runtime.script_exec("-app-", [exec]);
    let crash = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        runtime.set_state(&id, ContainerState::Exited(Some(137)));
    };
    let started = Instant::now();
    let probes = net(&runtime);
    let (result, ()) = tokio::join!(check.run(&probes, &id, &()), crash);
    let err = result.unwrap_err();
    assert!(matches!(err, HealthError::Exited { .. }), "{err:?}");
    // Noticed at the next look, after the exec's timeout and an interval; not the start period.
    assert_eq!(started.elapsed(), DEFAULT_EXEC_TIMEOUT + DEFAULT_INTERVAL);
}

#[tokio::test(start_paused = true)]
async fn a_deploy_probe_at_the_cap_gives_up_once_the_container_stops() {
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new()).unwrap();
    let (runtime, id) = running().await;
    let (a, _a) = hung();
    let (b, _b) = hung();
    let (c, _c) = hung();
    runtime.script_exec("-app-", [a, b, c]);
    let execs = ExecSlots::default();
    let probes = Probes::new(&runtime, &NetworkProber, &execs);
    for _ in 0..3 {
        check.live(&probes, &id).await.unwrap_err();
    }
    assert_eq!(execs.abandoned(&id), 3);

    // The deploy probe waits at the cap; the container stops 2 s in, its execs still hung.
    let crash = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        runtime.set_state(&id, ContainerState::Exited(Some(137)));
    };
    let started = Instant::now();
    let (result, ()) = tokio::join!(check.run(&probes, &id, &()), crash);
    let err = result.unwrap_err();
    assert!(matches!(err, HealthError::Exited { .. }), "{err:?}");
    assert!(
        started.elapsed() <= Duration::from_secs(3),
        "well before the 30 s start period: {:?}",
        started.elapsed()
    );
    assert_eq!(runtime.execs().len(), 3, "no fourth exec");
}

#[tokio::test]
async fn a_routed_exec_check_also_needs_the_tcp_connect() {
    let toml = format!("{EXEC}\ninterval = \"10ms\"\nstart_period = \"100ms\"")
        .replace("interval = \"1s\"\n", "")
        .replace("start_period = \"10s\"\n", "");
    let closed = ClosedPort::new();
    let check = Check::for_service(&service(&toml), &ports(8080, closed.port)).unwrap();
    let (runtime, id) = running().await;
    let err = check.run(&net(&runtime), &id, &()).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("last result: exit 0; TCP: cannot connect"),
        "{err}"
    );
    assert_eq!(
        check
            .live(&net(&runtime), &id)
            .await
            .unwrap_err()
            .split(';')
            .next(),
        Some("exit 0")
    );

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let check = Check::for_service(&service(&toml), &ports(8080, port)).unwrap();
    check.run(&net(&runtime), &id, &()).await.unwrap();
    assert_eq!(
        check.live(&net(&runtime), &id).await,
        Ok("exit 0; TCP: connected".to_owned())
    );
    runtime.script_exec("-app-", [exited(1, "")]);
    assert_eq!(
        check.live(&net(&runtime), &id).await,
        Err("exit 1; TCP: connected".to_owned())
    );
}

#[tokio::test]
async fn exec_output_goes_to_deployment_events_with_secrets_redacted() {
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
    let check = Check::for_service(&unrouted(EXEC), &PortMap::new())
        .unwrap()
        .with_interval(Duration::from_millis(5))
        .with_start_period(Duration::from_millis(20));
    let (runtime, id) = running().await;
    let output = format!(
        "{}connecting as postgres:hunter22 with key abc\nrefused\n",
        "x".repeat(400)
    );
    runtime.script_exec("-app-", [exited(2, &output)]);
    let secrets = [SecretValue::from("hunter22"), SecretValue::from("abc")];
    let log = DeploymentLog::new(
        store.clone(),
        deployment.id,
        DeploymentState::HealthChecking,
    )
    .redacting(&secrets);
    check.run(&net(&runtime), &id, &log).await.unwrap_err();

    let events = store.events(deployment.id).await.unwrap();
    let probe = events
        .iter()
        .filter_map(|e| e.message.as_deref())
        .find(|m| m.starts_with("health probe"))
        .unwrap();
    assert!(
        probe.ends_with("connecting as postgres:<redacted> with key abc\\nrefused\""),
        "{probe}"
    );
    assert!(!probe.contains("hunter22"), "{probe}");
    assert!(probe.contains("output \"…x"), "cut to its tail: {probe}");
    // The tail, its quotes and the ellipsis.
    let tail = probe.split_once("output ").unwrap().1;
    assert!(tail.chars().count() <= Tail::MAX_CHARS + 4, "{tail}");
}

#[tokio::test]
async fn a_secret_containing_another_is_redacted_whole() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let secrets = [
        SecretValue::from("hunter22"),
        SecretValue::from("hunter22-extended"),
        SecretValue::from("hunter22"),
    ];
    let log = DeploymentLog::new(
        store,
        DeploymentId::from(1),
        DeploymentState::HealthChecking,
    )
    .redacting(&secrets);
    assert_eq!(log.secrets.len(), 2, "duplicates dropped");
    assert_eq!(
        log.redact("as hunter22-extended, then hunter22"),
        "as <redacted>, then <redacted>"
    );
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
    check.run(&net(&runtime), &id, &()).await.unwrap();

    runtime.set_state(&id, ContainerState::Exited(Some(1)));
    let err = check.run(&net(&runtime), &id, &()).await.unwrap_err();
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
    check.run(&net(&runtime), &id, &log).await.unwrap_err();

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
