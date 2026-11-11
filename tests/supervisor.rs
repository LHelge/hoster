//! Spawns the real `hoster run` binary and checks its signal handling, systemd notifications and
//! exit codes. Podman is a [`FakePodman`] answering the startup handshake and reconciliation, the proxy listens on
//! an ephemeral loopback port, and every other phase that needs the network is still a stub, so
//! this runs offline.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Just enough of podman's compat API for startup steps 4 and 5: `/version` (used by version
/// negotiation and the startup log), `/_ping`, and empty container and network lists for the
/// startup reconciliation; one request per connection.
struct FakePodman;

/// The requests every [`FakePodman`] of this process has answered.
static PODMAN_REQUESTS: AtomicUsize = AtomicUsize::new(0);

impl FakePodman {
    const VERSION: &str = r#"{"Version":"6.1.4","ApiVersion":"1.44","MinAPIVersion":"1.24","Components":[{"Name":"Podman Engine","Version":"6.1.4"}]}"#;

    /// Serves on `socket` from a background thread for the rest of the test process.
    fn serve(socket: &Path) {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                Self::answer(stream);
            }
        });
    }

    fn answer(mut stream: UnixStream) {
        let mut reader = BufReader::new(&stream);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        // Skip the headers; these are bodiless GETs.
        let mut header = String::new();
        while reader.read_line(&mut header).is_ok_and(|n| n > 0) && header != "\r\n" {
            header.clear();
        }
        PODMAN_REQUESTS.fetch_add(1, Ordering::SeqCst);
        let target = request_line.split_whitespace().nth(1).unwrap_or("");
        let path = target.split('?').next().unwrap_or("");
        let (content_type, body) = if path.ends_with("/_ping") {
            ("text/plain", "OK")
        } else if path.ends_with("/version") {
            ("application/json", Self::VERSION)
        } else if path.ends_with("/containers/json") || path.ends_with("/networks") {
            ("application/json", "[]")
        } else {
            ("text/plain", "")
        };
        let status = if body.is_empty() {
            "404 Not Found"
        } else {
            "200 OK"
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
    }
}

/// A scratch directory with a minimal valid hoster.toml and a [`FakePodman`], removed on drop.
struct Sandbox {
    dir: PathBuf,
    /// The proxy's plain-HTTP listen address: a loopback port that was free a moment ago, from
    /// [`Sandbox::free_port`].
    http: SocketAddr,
    /// The proxy's TLS listen address, chosen the same way.
    https: SocketAddr,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        Self::with_busy_http(name).0
    }

    /// A sandbox whose HTTP port is taken: by the returned listener, for as long as it lives.
    fn with_busy_http(name: &str) -> (Self, TcpListener) {
        let dir = std::env::temp_dir().join(format!("hoster-it-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        FakePodman::serve(&dir.join("podman.sock"));
        let state_dir = dir.join("state");
        std::fs::write(
            dir.join("hoster.toml"),
            format!(
                "admin_host = \"hoster.example.com\"\nstate_dir = \"{}\"\n",
                state_dir.display()
            ),
        )
        .unwrap();
        // The GitHub OAuth App's credentials, as systemd would provide them.
        let credentials = dir.join("credentials");
        std::fs::create_dir_all(&credentials).unwrap();
        std::fs::write(credentials.join("github_client_id"), "test-client\n").unwrap();
        std::fs::write(credentials.join("github_client_secret"), "test-secret\n").unwrap();
        let (busy, https) = (Self::free_port(), Self::free_port());
        let (http, https) = (busy.local_addr().unwrap(), https.local_addr().unwrap());
        (Sandbox { dir, http, https }, busy)
    }

    /// A listener on a loopback port no other sandbox of this run was given.
    ///
    /// The port is not one the kernel picks (`127.0.0.1:0`): hoster binds it only after the
    /// listener is dropped, and until then the kernel may hand a port from its ephemeral range
    /// to any socket the tests running beside this one open, an outgoing connection included.
    /// Ports below that range are only taken by who asks for them by number, which here is
    /// this function, counting up from a start that differs between test processes.
    fn free_port() -> TcpListener {
        const RANGE: std::ops::Range<u16> = 10_000..20_000;
        static NEXT: AtomicU16 = AtomicU16::new(0);
        let start = (std::process::id() % 500) as u16 * 20;
        loop {
            let offset = start.wrapping_add(NEXT.fetch_add(1, Ordering::Relaxed));
            let port = RANGE.start + offset % (RANGE.end - RANGE.start);
            if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                return listener;
            }
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// `hoster run` with a clean environment pointing at this sandbox.
    fn command(&self) -> Command {
        self.configure(Command::new(env!("CARGO_BIN_EXE_hoster")))
    }

    /// [`Sandbox::command`] for a hoster that may hold at most `files` open file descriptors
    /// (`RLIMIT_NOFILE`, the unit's `LimitNOFILE=`).
    fn command_with_fd_limit(&self, files: u32) -> Command {
        let mut shell = Command::new("/bin/sh");
        shell
            .arg("-c")
            .arg(format!("ulimit -n {files} && exec \"$0\" \"$@\""))
            .arg(env!("CARGO_BIN_EXE_hoster"));
        self.configure(shell)
    }

    fn configure(&self, mut cmd: Command) -> Command {
        cmd.arg("run")
            .arg("--config")
            .arg(self.path("hoster.toml"))
            .env_clear()
            .env("HOME", &self.dir)
            .env("RUST_LOG", "info")
            .env("HOSTER_ADMIN_GITHUB", "octocat")
            .env("CREDENTIALS_DIRECTORY", self.path("credentials"))
            .env("HOSTER_PODMAN_SOCKET", self.path("podman.sock"))
            .env("HOSTER_PROXY__HTTP_LISTEN", self.http.to_string())
            .env("HOSTER_PROXY__HTTPS_LISTEN", self.https.to_string())
            .env("HOSTER_PROXY__THREADS", "1")
            .env("HOSTER_PROXY__GRACE_PERIOD", "1s")
            .env("HOSTER_PROXY__GRACEFUL_SHUTDOWN_TIMEOUT", "1s")
            // Nothing listens here: the ACME task must not reach a real CA from a test.
            .env(
                "HOSTER_ACME__DIRECTORY_URL",
                "https://127.0.0.1:1/directory",
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A running hoster whose stderr lines are forwarded to a channel.
struct Hoster {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Hoster {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd.spawn().expect("hoster binary starts");
        let stderr: ChildStderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[hoster] {line}");
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Hoster { child, lines }
    }

    /// Waits for a stderr line containing `needle`.
    fn wait_for_line(&self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(needle) => return,
                Ok(_) => {}
                Err(_) => panic!("hoster never logged {needle:?}"),
            }
        }
    }

    fn signal(&self, signal: Signal) {
        kill(Pid::from_raw(self.child.id() as i32), signal).expect("signal delivered");
    }

    /// Waits for the process to exit, killing it after the timeout.
    fn wait(mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        panic!("hoster did not exit within {TIMEOUT:?}");
    }
}

/// A stand-in for systemd's `NOTIFY_SOCKET`.
struct NotifySocket {
    socket: UnixDatagram,
}

impl NotifySocket {
    fn bind(path: &Path) -> Self {
        let socket = UnixDatagram::bind(path).unwrap();
        socket.set_read_timeout(Some(TIMEOUT)).unwrap();
        NotifySocket { socket }
    }

    fn recv(&self) -> String {
        let mut buf = [0u8; 1024];
        let n = self
            .socket
            .recv(&mut buf)
            .expect("notification within timeout");
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

#[test]
fn sigterm_notifies_systemd_and_exits_cleanly() {
    let sandbox = Sandbox::new("sigterm");
    let socket_path = sandbox.path("notify.sock");
    let notify = NotifySocket::bind(&socket_path);
    let mut cmd = sandbox.command();
    cmd.env("NOTIFY_SOCKET", &socket_path);
    let hoster = Hoster::spawn(cmd);

    assert_eq!(notify.recv().trim(), "READY=1");
    hoster.signal(Signal::SIGTERM);
    assert_eq!(notify.recv().trim(), "STOPPING=1");
    hoster.wait_for_line("mode=graceful");
    hoster.wait_for_line("hoster stopped");
    let status = hoster.wait();
    assert!(status.success(), "exit status {status:?}");
}

#[test]
fn sigint_without_systemd_shuts_down_fast() {
    let sandbox = Sandbox::new("sigint");
    let hoster = Hoster::spawn(sandbox.command());
    hoster.wait_for_line("hoster is ready");
    hoster.signal(Signal::SIGINT);
    hoster.wait_for_line("mode=fast");
    hoster.wait_for_line("hoster stopped");
    let status = hoster.wait();
    assert!(status.success(), "exit status {status:?}");
}

#[test]
fn startup_failure_exits_non_zero_with_a_clear_message() {
    let sandbox = Sandbox::new("fail");
    let output = sandbox
        .command()
        .env_remove("HOSTER_ADMIN_GITHUB")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"), "stderr: {stderr}");
    assert!(stderr.contains("HOSTER_ADMIN_GITHUB"), "stderr: {stderr}");
}

#[test]
fn startup_connects_to_podman_and_logs_versions() {
    let sandbox = Sandbox::new("podman");
    let hoster = Hoster::spawn(sandbox.command());
    hoster.wait_for_line("connected to podman");
    hoster.wait_for_line("hoster is ready");
    hoster.signal(Signal::SIGINT);
    assert!(hoster.wait().success());
}

#[test]
fn missing_podman_socket_fails_startup_and_points_at_doctor() {
    let sandbox = Sandbox::new("nopodman");
    let missing = sandbox.path("missing/podman.sock");
    let output = sandbox
        .command()
        .env("HOSTER_PODMAN_SOCKET", &missing)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("step 4"), "stderr: {stderr}");
    assert!(
        stderr.contains(&missing.display().to_string()),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("hoster doctor"), "stderr: {stderr}");
}

/// Sends one HTTP/1.1 request to `addr` and returns the raw response.
fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("proxy accepts connections");
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: app.example.com\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn proxy_answers_404_and_sigterm_drains_within_the_grace_period() {
    let sandbox = Sandbox::new("proxy");
    let hoster = Hoster::spawn(sandbox.command());
    hoster.wait_for_line("proxy is listening");
    hoster.wait_for_line("hoster is ready");

    for path in ["/", "/some/where?x=1"] {
        let response = http_get(sandbox.http, path);
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    }

    let stopping = Instant::now();
    hoster.signal(Signal::SIGTERM);
    hoster.wait_for_line("hoster stopped");
    let status = hoster.wait();
    assert!(status.success(), "exit status {status:?}");
    // grace_period (1s) + graceful_shutdown_timeout (1s), with slack for a loaded machine.
    let took = stopping.elapsed();
    assert!(took < Duration::from_secs(5), "shutdown took {took:?}");
    for addr in [sandbox.http, sandbox.https] {
        assert!(
            TcpStream::connect(addr).is_err(),
            "the listener on {addr} is closed after exit"
        );
    }
}

#[test]
fn a_busy_http_port_fails_startup_in_step_7() {
    let (sandbox, _busy) = Sandbox::with_busy_http("busy");
    let output = sandbox.command().output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("step 7"), "stderr: {stderr}");
    assert!(
        stderr.contains(&format!("cannot listen on {}", sandbox.http)),
        "stderr: {stderr}"
    );
}

#[test]
fn a_second_sigterm_during_the_grace_period_exits_promptly() {
    let sandbox = Sandbox::new("escalate");
    let mut cmd = sandbox.command();
    // Far longer than the test waits: the exit must not wait out Pingora's grace-period sleep.
    cmd.env("HOSTER_PROXY__GRACE_PERIOD", "60s");
    let hoster = Hoster::spawn(cmd);
    hoster.wait_for_line("hoster is ready");

    hoster.signal(Signal::SIGTERM);
    hoster.wait_for_line("phase=join proxy thread");
    let escalated = Instant::now();
    hoster.signal(Signal::SIGTERM);
    hoster.wait_for_line("escalating shutdown");
    hoster.wait_for_line("hoster stopped");
    let status = hoster.wait();
    assert!(status.success(), "exit status {status:?}");
    let took = escalated.elapsed();
    assert!(took < Duration::from_secs(6), "fast shutdown took {took:?}");
}

/// The admin app's status line for `/` with a session cookie it does not know, which it looks
/// up in SQLite; `None` when it does not answer within `wait`.
fn admin_lookup(sandbox: &Sandbox, wait: Duration) -> Option<String> {
    let mut stream = UnixStream::connect(sandbox.path("state/admin.sock")).ok()?;
    stream.set_read_timeout(Some(wait)).unwrap();
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: hoster.example.com\r\nCookie: __Host-hoster_session=00\r\n\
         Connection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    response.lines().next().map(str::to_owned)
}

/// Whether `:80` answers a request on a new connection; a refused one is closed unanswered.
fn plain_answers(addr: SocketAddr) -> bool {
    let mut stream = TcpStream::connect(addr).expect("the kernel accepts the connection");
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    let _ = write!(
        stream,
        "GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    );
    stream.read(&mut [0; 16]).is_ok_and(|n| n > 0)
}

/// Clients that hold more connections than hoster may have descriptors for (architecture §9.1,
/// ADR-0065): hoster refuses new clients at a quarter of its limit, so it never runs out.
/// Connections already open, the admin socket, SQLite and podman keep working, and new
/// clients are taken on again once connections close.
#[test]
fn new_clients_are_refused_at_the_descriptor_cap() {
    let sandbox = Sandbox::new("fdcap");
    // A hard limit of 512 keeps the soft one there: the cap is 128.
    let mut hoster = Hoster::spawn(sandbox.command_with_fd_limit(512));
    hoster.wait_for_line("connection_cap=128");
    hoster.wait_for_line("hoster is ready");

    let mut open = TcpStream::connect(sandbox.http).unwrap();
    open.set_read_timeout(Some(TIMEOUT)).unwrap();
    let kept_alive = |open: &mut TcpStream| {
        write!(open, "GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut buf = [0; 4096];
        let n = open
            .read(&mut buf)
            .expect("an answer on the open connection");
        String::from_utf8_lossy(&buf[..n]).into_owned()
    };
    assert!(kept_alive(&mut open).starts_with("HTTP/1.1 404"));

    // More than the cap, and short of the limit if hoster accepted them all.
    let held: Vec<TcpStream> = (0..400)
        .map(|i| {
            let addr = if i % 2 == 0 {
                sandbox.http
            } else {
                sandbox.https
            };
            TcpStream::connect(addr).expect("the kernel accepts the connection")
        })
        .collect();
    hoster.wait_for_line("refusing new client connections: too many open files");
    assert!(!plain_answers(sandbox.http), "a new client is refused");

    // Everything else keeps working: the open connection, the admin socket and SQLite, and
    // podman at the reconciler's next pass.
    assert!(kept_alive(&mut open).starts_with("HTTP/1.1 404"));
    assert_eq!(
        admin_lookup(&sandbox, TIMEOUT).as_deref(),
        Some("HTTP/1.1 303 See Other")
    );
    let podman = PODMAN_REQUESTS.load(Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(30);
    while PODMAN_REQUESTS.load(Ordering::SeqCst) == podman {
        assert!(Instant::now() < deadline, "no podman call at the cap");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        hoster.child.try_wait().unwrap(),
        None,
        "hoster keeps running"
    );

    drop(held);
    let recovered = Instant::now() + TIMEOUT;
    while !plain_answers(sandbox.http) {
        assert!(Instant::now() < recovered, "new clients are taken on again");
        std::thread::sleep(Duration::from_millis(100));
    }

    hoster.signal(Signal::SIGINT);
    assert!(hoster.wait().success());
}

/// What a scanner does to a public hoster, against the real binary: on `:80` and on `:443`,
/// with and without a TLS session.
struct Scanner<'a> {
    sandbox: &'a Sandbox,
    /// A client that trusts the certificate seeded for the admin host.
    roots: rustls::RootCertStore,
}

impl<'a> Scanner<'a> {
    const ADMIN_HOST: &'static str = "hoster.example.com";

    /// Seeds `sandbox` with a certificate for the admin host, which hoster loads at startup,
    /// so some of the scanner's TLS handshakes succeed.
    fn new(sandbox: &'a Sandbox) -> Self {
        let key = rcgen::generate_simple_self_signed(vec![Self::ADMIN_HOST.to_owned()]).unwrap();
        let dir = sandbox.path("state/certs").join(Self::ADMIN_HOST);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fullchain.pem"), key.cert.pem()).unwrap();
        std::fs::write(dir.join("privkey.pem"), key.signing_key.serialize_pem()).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(key.cert.der().clone()).unwrap();
        Scanner { sandbox, roots }
    }

    /// Writes `bytes` to `addr` and waits for the answer or the end of the connection.
    fn plain(&self, addr: SocketAddr, bytes: &[u8]) {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let _ = stream.write_all(bytes);
        let _ = stream.read(&mut [0; 1024]);
    }

    /// A TLS handshake on `:443` for `sni` (none: no SNI extension) offering `alpn`, then
    /// `bytes`. Returns what came back, empty when the handshake failed.
    fn tls(&self, sni: Option<&str>, alpn: &str, bytes: &[u8]) -> String {
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(self.roots.clone())
            .with_no_client_auth();
        config.enable_sni = sni.is_some();
        config.alpn_protocols = vec![alpn.as_bytes().to_vec()];
        let name = sni.unwrap_or("unnamed.invalid").to_owned();
        let name = rustls::pki_types::ServerName::try_from(name).unwrap();
        let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(config), name).unwrap();
        let mut tcp = TcpStream::connect(self.sandbox.https).unwrap();
        tcp.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut stream = rustls::Stream::new(&mut conn, &mut tcp);
        let mut answer = Vec::new();
        if stream.write_all(bytes).is_ok() {
            let _ = stream.read_to_end(&mut answer);
        }
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// One of everything that reaches no app.
    fn scan(&self) {
        let (http, https) = (self.sandbox.http, self.sandbox.https);
        let admin = Some(Self::ADMIN_HOST);
        // `:80`: an address as the host, a known host, a request that never finishes.
        self.plain(http, b"GET / HTTP/1.1\r\nHost: 203.0.113.9\r\n\r\n");
        self.plain(
            http,
            b"GET /.env HTTP/1.1\r\nHost: hoster.example.com\r\n\r\n",
        );
        self.plain(http, b"GET / HTTP/1.1\r\nHost: 203.0.");
        // `:443` without a TLS session: a port scan, plain HTTP, no SNI, a name without a
        // certificate.
        drop(TcpStream::connect(https).unwrap());
        self.plain(https, b"GET / HTTP/1.1\r\nHost: 203.0.113.9\r\n\r\n");
        assert_eq!(self.tls(None, "http/1.1", b"GET / HTTP/1.1\r\n\r\n"), "");
        assert_eq!(
            self.tls(
                Some("scanner.example.net"),
                "http/1.1",
                b"GET / HTTP/1.1\r\n\r\n"
            ),
            ""
        );
        // `:443` with one: another host (421), half a request, HTTP/1.1 on an `h2` connection.
        let misdirected = self.tls(
            admin,
            "http/1.1",
            b"GET / HTTP/1.1\r\nHost: 203.0.113.9\r\n\r\n",
        );
        assert!(misdirected.starts_with("HTTP/1.1 421"), "{misdirected}");
        self.tls(admin, "http/1.1", b"GET / HTTP/1.1\r\nHost: hoster.exa");
        self.tls(
            admin,
            "h2",
            b"GET / HTTP/1.1\r\nHost: hoster.example.com\r\n\r\n",
        );
    }

    /// A request for the admin app, whose access line marks a point in the log.
    fn request(&self, path: &str) {
        let head =
            format!("GET {path} HTTP/1.1\r\nHost: hoster.example.com\r\nConnection: close\r\n\r\n");
        let answer = self.tls(Some(Self::ADMIN_HOST), "http/1.1", head.as_bytes());
        assert!(answer.starts_with("HTTP/1.1 "), "{answer}");
    }
}

impl Hoster {
    /// The stderr lines up to the first one containing `needle`.
    fn lines_until(&self, needle: &str) -> Vec<String> {
        let deadline = Instant::now() + TIMEOUT;
        let mut lines = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(needle) => return lines,
                Ok(line) => lines.push(line),
                Err(_) => panic!("hoster never logged {needle:?}"),
            }
        }
    }
}

/// With the default `RUST_LOG=info`, nothing a scanner does writes a line: not the requests
/// hoster answers itself, and not Pingora's `error` for each connection the client broke.
/// A request for a routed host still gets its access line.
#[test]
fn scanner_traffic_writes_no_log_line_by_default() {
    let sandbox = Sandbox::new("scanner");
    let scanner = Scanner::new(&sandbox);
    let hoster = Hoster::spawn(sandbox.command());
    hoster.wait_for_line("hoster is ready");

    scanner.scan();
    // hoster logs a broken connection when it notices, which can be after the client is done.
    std::thread::sleep(Duration::from_millis(300));
    scanner.request("/login?end-of-scan");

    let during = hoster.lines_until("path=\"/login\"");
    let from_the_proxy: Vec<&String> = during
        .iter()
        .filter(|line| {
            line.contains("pingora")
                || line.contains("hoster::access")
                || line.contains("hoster::proxy")
        })
        .collect();
    assert_eq!(from_the_proxy, Vec::<&String>::new());

    hoster.signal(Signal::SIGINT);
    assert!(hoster.wait().success());
}

/// The same scan with the two `debug` targets on shows what was held back, each line under
/// its target. This is also what notices a Pingora upgrade that words its errors differently
/// (`ClientNoise` in `src/logging.rs` recognises them by their start).
#[test]
fn scanner_traffic_is_logged_at_debug_under_its_own_targets() {
    let sandbox = Sandbox::new("scanner-debug");
    let scanner = Scanner::new(&sandbox);
    let mut cmd = sandbox.command();
    cmd.env(
        "RUST_LOG",
        "info,hoster::access::unrouted=debug,hoster::proxy::client=debug",
    );
    let hoster = Hoster::spawn(cmd);
    hoster.wait_for_line("hoster is ready");

    scanner.scan();
    std::thread::sleep(Duration::from_millis(300));
    scanner.request("/login?end-of-scan");

    let during = hoster.lines_until("path=\"/login\"");
    let count = |target: &str, needle: &str| {
        let target = format!("DEBUG {target}: ");
        during
            .iter()
            .filter(|line| line.contains(&target) && line.contains(needle))
            .count()
    };
    // Two answered on `:80` and the 421.
    assert_eq!(
        count("hoster::access::unrouted", "request"),
        3,
        "{during:#?}"
    );
    assert_eq!(
        count("hoster::access::unrouted", "status=421"),
        1,
        "{during:#?}"
    );
    // Four failed TLS handshakes, one failed HTTP/2 handshake, two requests cut short.
    assert_eq!(
        count("hoster::proxy::client", "Downstream handshake error"),
        4,
        "{during:#?}"
    );
    assert_eq!(
        count("hoster::proxy::client", "H2 handshake error"),
        1,
        "{during:#?}"
    );
    assert_eq!(
        count("hoster::proxy::client", "Fail to proxy: Downstream"),
        2,
        "{during:#?}"
    );
    assert!(
        !during
            .iter()
            .any(|line| line.contains("ERROR") && line.contains("pingora")),
        "{during:#?}"
    );

    hoster.signal(Signal::SIGINT);
    assert!(hoster.wait().success());
}
