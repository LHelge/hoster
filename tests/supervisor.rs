//! Spawns the real `hoster run` binary and checks its signal handling, systemd notifications and
//! exit codes. Podman is a [`FakePodman`] answering the startup handshake, the proxy listens on
//! an ephemeral loopback port, and every other phase that needs the network is still a stub, so
//! this runs offline.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Just enough of podman's compat API for startup step 4: `/version` (used by version
/// negotiation and the startup log) and `/_ping`, one request per connection.
struct FakePodman;

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
        let path = request_line.split_whitespace().nth(1).unwrap_or("");
        let (content_type, body) = if path.ends_with("/_ping") {
            ("text/plain", "OK")
        } else if path.ends_with("/version") {
            ("application/json", Self::VERSION)
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
    /// The proxy's plain-HTTP listen address: a loopback port that was free a moment ago.
    http: SocketAddr,
}

impl Sandbox {
    fn new(name: &str) -> Self {
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
        let http = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        Sandbox { dir, http }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// `hoster run` with a clean environment pointing at this sandbox.
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_hoster"));
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
            .env("HOSTER_PROXY__THREADS", "1")
            .env("HOSTER_PROXY__GRACE_PERIOD", "1s")
            .env("HOSTER_PROXY__GRACEFUL_SHUTDOWN_TIMEOUT", "1s")
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
    assert!(
        TcpStream::connect(sandbox.http).is_err(),
        "the listener is closed after exit"
    );
}

#[test]
fn a_busy_http_port_fails_startup_in_step_7() {
    let sandbox = Sandbox::new("busy");
    let _busy = TcpListener::bind(sandbox.http).unwrap();
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
