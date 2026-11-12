//! End-to-end zero-downtime deploy under load (ADR-0017), on real rootless podman.
//!
//! hoster's deploy path is assembled in-process the way the supervisor wires it: a real
//! [`Store`], [`PodmanRuntime`], [`PortAllocator`] and [`Orchestrator`] with the network prober,
//! publishing into the `ArcSwap<RouteTable>` of a real Pingora [`Proxy`] that serves `:443` with
//! a certificate from a test CA. The test then drives it like the admin UI does, through
//! [`Orchestrator::apply`]:
//!
//! 1. apply v1 of a one-service stack (busybox `httpd` answering its version on `/`, plus a CGI
//!    stream on `/cgi-bin/stream`) and wait for it to be live;
//! 2. start continuous load through the proxy: an HTTP/1.1 keep-alive client, an HTTP/2 client
//!    and an HTTP/1.1 client opening a new connection (and TLS session) per request, plus one
//!    long streaming request on v1;
//! 3. apply v2 (only `VERSION` changes, so the service is replaced start-first);
//! 4. apply a broken v3 whose `/healthz` is missing, so its health check fails;
//! 5. stop the load and check: no error and no non-2xx response at any point, every client saw
//!    v1 then v2 and never went back, the stream finished whole on v1 and the v2 deployment
//!    waited for it (the drain), and v3 never answered a request.
//!
//! Skipped unless `TEST_HOSTER_PODMAN=1`, like the other podman tests; run it with
//!
//! ```sh
//! systemctl --user start podman.socket
//! TEST_HOSTER_PODMAN=1 cargo test e2e_tests -- --nocapture
//! ```
//!
//! `HOSTER_PODMAN_SOCKET` overrides the socket path. The image is busybox pinned by digest
//! (pulled from Docker Hub on first use); the "versions" are an environment variable.

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora::server::ShutdownSignal;
use tempfile::TempDir;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::acme::{CertMap, ChallengeMap};
use crate::config::{PortRange, ProxyConfig};
use crate::orchestrator::{Orchestrator, RemoveStack, Settings, TagResolver};
use crate::ports::PortAllocator;
use crate::proxy::tests::TestCa;
use crate::proxy::{Proxy, ProxyState};
use crate::registry::RegistryError;
use crate::routing::RouteTable;
use crate::runtime::{PodmanRuntime, PodmanSocket};
use crate::secrets::{SecretKey, SecretStore};
use crate::stack::{Digest, ImageRef, PinnedImage};
use crate::store::{Deployment, DeploymentState, Store};

/// busybox (it has `httpd` with CGI), pinned by digest so the orchestrator never resolves a tag.
const IMAGE: &str = "docker.io/library/busybox:1.37@sha256:66a6306db78bf2dbf3487f293aa8d6990d8e506fdffab9cc43fe422becf886e4";

/// The routed host; the proxy serves a test-CA certificate for it.
const HOST: &str = "zero-downtime.example.com";

/// Lines the streaming request sends, one every [`TICK`].
const TICKS: usize = 16;
const TICK: &str = "0.5";

/// How long a deployment may take before the test gives up on it.
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(120);

fn enabled() -> bool {
    std::env::var("TEST_HOSTER_PODMAN").is_ok_and(|v| v == "1")
}

/// Image tags never need resolving here: the only image is pinned by digest.
struct NoRegistry;

#[async_trait]
impl TagResolver for NoRegistry {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError> {
        panic!("the end-to-end test resolves no tags, but was asked for {image}");
    }

    async fn contains(&self, image: &ImageRef, digest: &Digest) -> Result<(), RegistryError> {
        panic!("the end-to-end test looks up no digests, but was asked for {image}@{digest}");
    }
}

/// One release of the test app.
#[derive(Debug, Clone, Copy)]
struct Release {
    version: &'static str,
    /// Whether the release serves `/healthz`; a broken one answers it with 404.
    healthy: bool,
}

impl Release {
    const V1: Release = Release {
        version: "v1",
        healthy: true,
    };
    const V2: Release = Release {
        version: "v2",
        healthy: true,
    };
    const BROKEN_V3: Release = Release {
        version: "v3",
        healthy: false,
    };

    /// The container's shell script: writes the site, then runs `httpd` in the foreground of
    /// a shell that exits on SIGTERM (`httpd` as PID 1 would ignore it until the stop timeout).
    fn script() -> String {
        format!(
            r#"set -e
mkdir -p /www/cgi-bin
echo "$VERSION" > /www/index.html
if [ "$HEALTHY" = 1 ]; then echo ok > /www/healthz; fi
cat > /www/cgi-bin/stream <<'CGI'
#!/bin/sh
printf 'Content-Type: text/plain\r\n\r\n'
i=0
while [ $i -lt {TICKS} ]; do i=$((i + 1)); echo "$VERSION tick $i"; sleep {TICK}; done
CGI
chmod +x /www/cgi-bin/stream
httpd -f -p 8080 -h /www &
trap 'kill $!; exit 0' TERM
wait"#
        )
    }

    /// The stack file deploying this release as stack `stack`.
    fn stack(self, stack: &str) -> String {
        let Release { version, healthy } = self;
        let healthy = u8::from(healthy);
        let script = Self::script();
        format!(
            r#"version = 1
name = "{stack}"
drain_timeout = "60s"

[services.app]
image = "{IMAGE}"
command = ["sh", "-c", '''{script}''']
stop_timeout = "2s"

[services.app.env]
VERSION = "{version}"
HEALTHY = "{healthy}"

[services.app.health]
http = {{ path = "/healthz", port = 8080 }}
interval = "100ms"
start_period = "5s"

[[services.app.routes]]
host = "{HOST}"
port = 8080
"#
        )
    }
}

/// hoster's deploy path and proxy, in-process, against real podman.
struct Hoster {
    _dir: TempDir,
    stack: String,
    orchestrator: Arc<Orchestrator<PodmanRuntime>>,
    /// The proxy's `:443` listener.
    https: SocketAddr,
    ca: TestCa,
    shutdown: watch::Sender<Option<ShutdownSignal>>,
}

impl Hoster {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(&dir.path().join("state")).await.unwrap();
        let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(PathBuf::from);
        let runtime = PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
            .await
            .expect("connect to rootless podman");
        let routes = Arc::<ArcSwap<RouteTable>>::default();
        let orchestrator = Orchestrator::new(
            store.clone(),
            Arc::new(runtime),
            Arc::new(NoRegistry),
            PortAllocator::new(store.clone(), Self::port_range()),
            SecretStore::new(store, &SecretKey::generate()),
            Settings::default(),
            Arc::clone(&routes),
        );
        orchestrator.restore_routes().await.unwrap();

        let ca = TestCa::new();
        let (certs, _) = CertMap::open(&dir.path().join("state"), None).unwrap();
        let (chain, key) = ca.issue(HOST);
        certs.insert(HOST, chain, key).unwrap();
        let config = ProxyConfig {
            http_listen: Self::free_port(),
            https_listen: Self::free_port(),
            threads: NonZeroUsize::new(2).unwrap(),
            ..ProxyConfig::default()
        };
        let state = ProxyState {
            routes,
            certs,
            challenges: ChallengeMap::default(),
            traffic: Default::default(),
            admission: Default::default(),
            logs: crate::logs::LogSink::disconnected(),
        };
        let (shutdown, rx) = watch::channel(None);
        let mut proxy = Proxy::new(&config, state).spawn(rx).unwrap();
        proxy.events.ready(Duration::from_secs(10)).await.unwrap();

        Hoster {
            _dir: dir,
            stack: Self::unique_stack(),
            orchestrator,
            https: config.https_listen,
            ca,
            shutdown,
        }
    }

    /// A stack name no other run uses, so parallel runs on one podman do not collide.
    fn unique_stack() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("e2e-{:x}-{nanos:x}", std::process::id())
    }

    /// Twenty upstream ports picked by process id, away from hoster's default range, so a
    /// hoster running on this machine or a parallel run is unlikely to hold them.
    fn port_range() -> PortRange {
        let start = 40000 + (std::process::id() % 1000) as u16 * 20;
        PortRange::try_from(start..=start + 19).unwrap()
    }

    fn free_port() -> SocketAddr {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
    }

    /// Applies `release` and waits until its deployment is over.
    async fn apply(&self, release: Release) -> Deployment {
        let outcome = self
            .orchestrator
            .apply(&release.stack(&self.stack))
            .await
            .unwrap_or_else(|err| panic!("applying {release:?}: {err}"));
        let id = outcome
            .deployment
            .expect("the release changes the stack")
            .deployment;
        tokio::time::timeout(DEPLOY_TIMEOUT, self.orchestrator.wait(id))
            .await
            .unwrap_or_else(|_| panic!("deploying {release:?} took over {DEPLOY_TIMEOUT:?}"))
            .unwrap()
    }

    /// A client for `https://HOST` through the proxy, offering `alpn`.
    fn client(&self, alpn: &[&[u8]], keep_alive: bool) -> reqwest::Client {
        let mut builder = reqwest::Client::builder()
            .tls_backend_preconfigured(self.ca.client_config(alpn))
            .resolve(HOST, self.https)
            .timeout(Duration::from_secs(30));
        if !keep_alive {
            builder = builder.pool_max_idle_per_host(0);
        }
        builder.build().unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("https://{HOST}:{}{path}", self.https.port())
    }

    /// The version the app answers on `/` right now.
    async fn version(&self) -> String {
        let response = self
            .client(&[b"http/1.1"], false)
            .get(self.url("/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.text().await.unwrap().trim().to_owned()
    }

    /// Removes the stack's containers and network, and stops the proxy.
    async fn clean_up(&self) {
        let removed = self
            .orchestrator
            .remove(RemoveStack::keeping_volumes(&self.stack))
            .await;
        if let Err(err) = removed {
            eprintln!("cleaning up stack {}: {err}", self.stack);
        }
        self.shutdown
            .send_replace(Some(ShutdownSignal::FastShutdown));
    }
}

/// How a load client connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connection {
    /// HTTP/1.1, reusing pooled connections.
    KeepAlive,
    /// HTTP/2, multiplexed over one connection.
    H2,
    /// HTTP/1.1 with a fresh TCP connection and TLS handshake per request.
    New,
}

impl Connection {
    const ALL: [Connection; 3] = [Connection::KeepAlive, Connection::H2, Connection::New];

    fn client(self, hoster: &Hoster) -> reqwest::Client {
        match self {
            Connection::KeepAlive => hoster.client(&[b"http/1.1"], true),
            Connection::H2 => hoster.client(&[b"h2"], true),
            Connection::New => hoster.client(&[b"http/1.1"], false),
        }
    }

    fn expected_version(self) -> reqwest::Version {
        match self {
            Connection::H2 => reqwest::Version::HTTP_2,
            Connection::KeepAlive | Connection::New => reqwest::Version::HTTP_11,
        }
    }
}

/// One request: the trimmed body of a 2xx answer, or why it failed, and when it completed.
#[derive(Debug)]
struct Answer {
    body: Result<String, String>,
    at: Instant,
}

/// What one client saw, in order.
#[derive(Debug)]
struct Log {
    connection: Connection,
    answers: Vec<Answer>,
}

impl Log {
    /// Every request answered 2xx, over the expected protocol.
    fn assert_no_errors(&self) {
        let errors: Vec<_> = self
            .answers
            .iter()
            .filter_map(|a| a.body.as_ref().err())
            .collect();
        assert!(
            errors.is_empty(),
            "{:?} client: {} of {} requests failed: {errors:#?}",
            self.connection,
            errors.len(),
            self.answers.len()
        );
    }

    /// The versions answered, with consecutive repeats collapsed: `[v1, v2]` for a clean
    /// switch.
    fn versions(&self) -> Vec<&str> {
        let mut versions: Vec<&str> = Vec::new();
        for answer in self.answers.iter().filter_map(|a| a.body.as_deref().ok()) {
            if versions.last() != Some(&answer) {
                versions.push(answer);
            }
        }
        versions
    }

    /// When this client first got an answer from `version`.
    fn first(&self, version: &str) -> Option<Instant> {
        self.answers
            .iter()
            .find(|a| a.body.as_deref().is_ok_and(|body| body == version))
            .map(|a| a.at)
    }
}

/// Continuous requests to `/` from one client per [`Connection`] kind until stopped.
struct Load {
    stop: Arc<AtomicBool>,
    clients: Vec<JoinHandle<Log>>,
}

impl Load {
    fn start(hoster: &Hoster) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let url = hoster.url("/");
        let clients = Connection::ALL
            .into_iter()
            .map(|connection| {
                let client = connection.client(hoster);
                let (stop, url) = (Arc::clone(&stop), url.clone());
                tokio::spawn(async move {
                    let mut answers = Vec::new();
                    while !stop.load(Ordering::SeqCst) {
                        let body = Self::request(&client, &url, connection).await;
                        answers.push(Answer {
                            body,
                            at: Instant::now(),
                        });
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    Log {
                        connection,
                        answers,
                    }
                })
            })
            .collect();
        Load { stop, clients }
    }

    async fn request(
        client: &reqwest::Client,
        url: &str,
        connection: Connection,
    ) -> Result<String, String> {
        let response = client.get(url).send().await.map_err(|e| format!("{e:?}"))?;
        let (status, version) = (response.status(), response.version());
        let body = response.text().await.map_err(|e| format!("body: {e:?}"))?;
        if !status.is_success() {
            return Err(format!("HTTP {status}: {body}"));
        }
        if version != connection.expected_version() {
            return Err(format!("answered over {version:?}"));
        }
        Ok(body.trim().to_owned())
    }

    async fn stop(self) -> Vec<Log> {
        self.stop.store(true, Ordering::SeqCst);
        let mut logs = Vec::new();
        for client in self.clients {
            logs.push(client.await.unwrap());
        }
        logs
    }
}

/// The long streaming request: its lines and when the last one arrived.
struct Stream {
    lines: Vec<String>,
    finished: Instant,
}

impl Stream {
    /// Starts `GET /cgi-bin/stream` and returns once the response head arrived, so the request
    /// is in flight on the current upstream; the rest is read in the background.
    async fn start(hoster: &Hoster) -> JoinHandle<Result<Stream, String>> {
        let mut response = hoster
            .client(&[b"http/1.1"], false)
            .get(hoster.url("/cgi-bin/stream"))
            .send()
            .await
            .expect("the stream starts");
        assert_eq!(response.status(), 200);
        tokio::spawn(async move {
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("stream cut after {} bytes: {e:?}", body.len()))?
            {
                body.extend_from_slice(&chunk);
            }
            let lines = String::from_utf8(body)
                .map_err(|e| e.to_string())?
                .lines()
                .map(str::to_owned)
                .collect();
            Ok(Stream {
                lines,
                finished: Instant::now(),
            })
        })
    }
}

/// The scenario; cleanup runs whether it passes or panics.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deploys_without_dropping_a_request_under_load() {
    if !enabled() {
        eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to run against rootless podman");
        return;
    }
    // The process-wide subscriber is shared with the log-capturing tests; installing a second
    // one here would make theirs fail when both run in the same `cargo test`.
    crate::logging::capture::global();
    let hoster = Arc::new(Hoster::start().await);
    let scenario = tokio::spawn(scenario(Arc::clone(&hoster))).await;
    hoster.clean_up().await;
    if let Err(err) = scenario {
        std::panic::resume_unwind(err.into_panic());
    }
}

async fn scenario(hoster: Arc<Hoster>) {
    let first = hoster.apply(Release::V1).await;
    assert_eq!(first.state, DeploymentState::Live, "{first:?}");
    assert_eq!(hoster.version().await, "v1");

    let load = Load::start(&hoster);
    let stream = Stream::start(&hoster).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let second = hoster.apply(Release::V2).await;
    let switched = Instant::now();
    assert_eq!(second.state, DeploymentState::Live, "{second:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let broken = hoster.apply(Release::BROKEN_V3).await;
    assert_eq!(broken.state, DeploymentState::Failed, "{broken:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let logs = load.stop().await;
    let stream = stream.await.unwrap().expect("the stream is not cut");

    for log in &logs {
        log.assert_no_errors();
        assert_eq!(
            log.versions(),
            ["v1", "v2"],
            "{:?} client: answers must switch from v1 to v2 once, and never reach v3",
            log.connection
        );
        assert!(
            log.answers.len() >= 20,
            "{:?} client sent only {} requests",
            log.connection,
            log.answers.len()
        );
    }

    let expected: Vec<String> = (1..=TICKS).map(|i| format!("v1 tick {i}")).collect();
    assert_eq!(stream.lines, expected, "the stream on v1 finishes whole");
    // The drain is only exercised if the stream was still running on v1 after the proxy had
    // switched to v2; a stream that ended before the swap would pass the next check vacuously.
    for log in &logs {
        let first_v2 = log.first("v2").expect("every client saw v2");
        assert!(
            stream.finished > first_v2,
            "{:?} client saw v2 only after the v1 stream had ended, so nothing was drained",
            log.connection
        );
    }
    assert!(
        stream.finished <= switched,
        "the v2 deployment finished before the v1 stream did: the old upstream was not drained"
    );

    assert_eq!(
        hoster.version().await,
        "v2",
        "v2 keeps serving after v3 failed"
    );
}

/// A container printing a JSON line on stdout and a text line on stderr, then idling.
fn printing_stack(stack: &str) -> String {
    format!(
        r#"version = 1
name = "{stack}"

[services.printer]
image = "{IMAGE}"
command = ["sh", "-c", '''echo '{{"level":"warn","msg":"hello from e2e","n":1}}'; echo 'plain text' >&2; exec sleep 3600''']
stop_timeout = "1s"
"#
    )
}

/// ADR-0067 §1: what a container hoster starts prints reaches the log store, parsed, with the
/// labels' stack, service and generation and its stream, and its follower ends at removal.
/// After a restart, a follower seeded from the stored cursor reads the log again and stores
/// nothing twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn container_output_reaches_the_log_store() {
    use crate::logs::{LogFollowers, LogRecorder};
    use crate::store::{Level, LogQuery, LogScope, LogStore, Stream as LogStream, Timestamp};

    if !enabled() {
        eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to run against rootless podman");
        return;
    }
    crate::logging::capture::global();
    let hoster = Arc::new(Hoster::start().await);
    let dir = TempDir::new().unwrap();
    let logs = LogStore::open(dir.path()).await.unwrap();
    let (recorder, sink) = LogRecorder::new(logs.clone()).await.unwrap();
    let stop = tokio_util::sync::CancellationToken::new();
    let recording = tokio::spawn(recorder.run(stop.clone()));
    let followers = hoster
        .orchestrator
        .connect_logs(sink.clone(), tokio_util::sync::CancellationToken::new());

    let scenario = {
        let (hoster, logs, followers) = (Arc::clone(&hoster), logs.clone(), followers.clone());
        let sink = sink.clone();
        async move {
            let applied = hoster
                .orchestrator
                .apply(&printing_stack(&hoster.stack))
                .await
                .unwrap();
            let id = applied.deployment.expect("a new stack deploys").deployment;
            let deployed = tokio::time::timeout(DEPLOY_TIMEOUT, hoster.orchestrator.wait(id))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(deployed.state, DeploymentState::Live, "{deployed:?}");

            let query = LogQuery::new(
                Timestamp::from_millis(0)..Timestamp::now() + Duration::from_secs(3600),
                LogScope::Stack(hoster.stack.clone()),
            );
            let records = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let page = logs.query(&query).await.unwrap();
                    if page.entries.len() >= 2 {
                        break page.entries;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("both lines are stored");
            let mut records: Vec<_> = records.into_iter().map(|e| e.record).collect();
            records.sort_by_key(|r| r.stream == LogStream::Stderr);
            let [json, text] = &records[..] else {
                panic!("two records: {records:?}");
            };
            assert_eq!(json.service, "printer");
            assert_eq!(json.generation, Some(1));
            assert_eq!(json.stream, LogStream::Stdout);
            assert_eq!(json.level, Some(Level::Warn));
            assert_eq!(json.message, "hello from e2e");
            assert_eq!(json.fields.as_deref(), Some(r#"{"n":1}"#));
            assert_eq!(text.stream, LogStream::Stderr);
            assert_eq!(text.level, None);
            assert_eq!(text.message, "plain text");
            assert_eq!(followers.followed().len(), 1);
            let container = followers.followed().remove(0);

            // hoster restarts: the stored cursor seeds the next follower, which reads the log
            // from its top again, past both lines, and stores neither twice.
            followers.stop().await;
            tokio::time::timeout(Duration::from_secs(10), async {
                while logs.cursor(container.as_ref()).await.unwrap().is_none() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("the cursor is stored");
            let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(PathBuf::from);
            let runtime = PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
                .await
                .unwrap();
            let stop = tokio_util::sync::CancellationToken::new();
            let resumed = LogFollowers::new(Arc::new(runtime), sink, stop.clone());
            assert_eq!(resumed.resume_from(&logs).await.unwrap(), 1);
            resumed.follow(&container);
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(resumed.followed(), [container]);
            assert_eq!(logs.query(&query).await.unwrap().entries.len(), 2);
            resumed.stop().await;
        }
    };
    let outcome = tokio::spawn(scenario).await;
    hoster.clean_up().await;
    if outcome.is_ok() {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !followers.followed().is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the follower ends with its container");
    }
    followers.stop().await;
    stop.cancel();
    recording.await.unwrap();
    if let Err(err) = outcome {
        std::panic::resume_unwind(err.into_panic());
    }
}
