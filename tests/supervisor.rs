//! Spawns the real `hoster run` binary and checks its signal handling, systemd notifications and
//! exit codes. Every startup phase that needs podman or the network is still a stub, so this
//! runs offline.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

const TIMEOUT: Duration = Duration::from_secs(10);

/// A scratch directory with a minimal valid hoster.toml, removed on drop.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("hoster-it-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state_dir = dir.join("state");
        std::fs::write(
            dir.join("hoster.toml"),
            format!(
                "admin_host = \"hoster.example.com\"\nstate_dir = \"{}\"\n",
                state_dir.display()
            ),
        )
        .unwrap();
        Sandbox { dir }
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
