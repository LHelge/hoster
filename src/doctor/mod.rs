//! `hoster doctor`: preflight checks that the host is set up the way ADR-0023 expects (a systemd
//! user service with linger, the user `podman.socket`, a lowered unprivileged-port start), so a
//! misconfiguration is diagnosed before `hoster run`.
//!
//! [`Doctor`] holds everything a check looks at — the uid, `/proc/sys`, the cgroup tree, the
//! podman socket candidates, the ports to bind, the running binary and its versioned layout
//! (ADR-0052) and the external tools (`loginctl`, `systemctl`,
//! `timedatectl`) behind [`Commands`] — so tests inject fakes for each. [`Doctor::diagnose`] runs every check
//! and returns a [`Report`], which renders as text ([`Display`](fmt::Display)) or JSON
//! ([`Serialize`]). Nothing here needs root; it runs as the service user.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Ipv6Addr, TcpListener};
use std::ops::RangeInclusive;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::config::{Config, ConfigError, PortRange};
use crate::installation::Installation;
use crate::runtime::PodmanSocket;

/// First systemd release with user-scoped encrypted credentials (`systemd-creds --user`).
const USER_CREDENTIALS_SYSTEMD: u32 = 256;
/// Ports hoster binds for HTTP and HTTPS, lowest first.
const PUBLIC_PORTS: [u16; 2] = [80, 443];
/// How long the podman `/_ping` may take.
const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// Most bytes read while looking for the `/_ping` status line.
const PING_MAX_STATUS_LINE: usize = 4096;
/// Controllers the user manager must delegate before stacks can get `Memory`, `NanoCpus` and
/// `PidsLimit` (ADR-0038).
const DELEGATED_CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];

/// Runs external programs for the checks; faked in tests.
pub trait Commands {
    /// Runs `program` with `args` and returns its trimmed standard output. A non-zero exit
    /// status is not an error: `systemctl is-enabled` reports "disabled" that way.
    fn stdout(&self, program: &str, args: &[&str]) -> io::Result<String>;
}

/// [`Commands`] that spawns real processes.
#[derive(Debug, Default, Clone)]
pub struct SystemCommands {
    /// `XDG_RUNTIME_DIR` for the children when this process has none (e.g. under
    /// `sudo -u hoster`), so `systemctl --user` still finds the user manager.
    runtime_dir: Option<PathBuf>,
}

impl Commands for SystemCommands {
    fn stdout(&self, program: &str, args: &[&str]) -> io::Result<String> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(dir) = &self.runtime_dir {
            command.env("XDG_RUNTIME_DIR", dir);
        }
        let output = command.output()?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if stdout.is_empty() && !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(io::Error::other(if stderr.is_empty() {
                format!("{program} exited with {}", output.status)
            } else {
                stderr
            }));
        }
        Ok(stdout)
    }
}

/// The host as the checks see it.
pub struct Doctor {
    /// The user the checks run as.
    uid: u32,
    /// Normally `/proc/sys`.
    proc_sys: PathBuf,
    /// Normally `/sys/fs/cgroup` (cgroup v2).
    cgroup_root: PathBuf,
    /// Podman socket locations, in the order bollard probes them.
    sockets: Vec<PathBuf>,
    /// Ports that must be bindable (80 and 443).
    ports: Vec<u16>,
    /// The running binary.
    exe: Option<PathBuf>,
    /// The versioned binary layout it should run from (ADR-0052); `None` without `$HOME`.
    installation: Option<Installation>,
    commands: Box<dyn Commands>,
}

impl Doctor {
    /// The real host: this process's uid, `/proc/sys`, `$XDG_RUNTIME_DIR` and system tools.
    pub fn from_env() -> io::Result<Self> {
        // The owner of /proc/self is this process's (effective) uid; no libc needed.
        let uid = std::fs::metadata("/proc/self")?.uid();
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let commands = SystemCommands {
            runtime_dir: runtime_dir
                .is_none()
                .then(|| PathBuf::from(format!("/run/user/{uid}"))),
        };
        Ok(Self {
            uid,
            proc_sys: PathBuf::from("/proc/sys"),
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            sockets: PodmanSocket::discover(runtime_dir.as_deref(), Some(uid))
                .candidates()
                .to_vec(),
            ports: PUBLIC_PORTS.to_vec(),
            exe: std::env::current_exe().ok(),
            installation: Installation::from_env(),
            commands: Box::new(commands),
        })
    }

    /// Runs every check. `config` is the result of loading the configuration; checks that need
    /// it are skipped when it failed.
    pub fn diagnose(&self, config: &Result<Config, ConfigError>) -> Report {
        let config = config.as_ref();
        Report {
            checks: vec![
                Check::required("user", self.check_user()),
                Check::required("config", Self::check_config(config)),
                Check::required(
                    "state_dir",
                    Self::with_config(config, |c| self.check_state_dir(&c.state_dir)),
                ),
                Check::required(
                    "podman_socket",
                    self.check_podman_socket(config.ok().and_then(|c| c.podman_socket.as_deref())),
                ),
                Check::required("unprivileged_ports", self.check_unprivileged_port_start()),
                Check::required("bind_ports", self.check_bind_ports()),
                Check::required("linger", self.check_linger()),
                Check::required("podman_service", self.check_podman_service()),
                Check::advisory("cgroup_delegation", self.check_cgroup_delegation()),
                Check::advisory("user_credentials", self.check_user_credentials()),
                Check::advisory("unit_sandbox", self.check_unit_sandbox()),
                // ADR-0026: the range "must stay outside" the ephemeral range.
                Check::required(
                    "port_range",
                    Self::with_config(config, |c| self.check_port_range(c.upstream_ports)),
                ),
                Check::required("clock", self.check_clock()),
                // A manual install still works; it just cannot update itself.
                Check::advisory("binary_layout", self.check_binary_layout()),
            ],
        }
    }

    fn with_config(
        config: Result<&Config, &ConfigError>,
        check: impl FnOnce(&Config) -> Outcome,
    ) -> Outcome {
        match config {
            Ok(config) => check(config),
            Err(_) => Outcome::Skip("needs a valid configuration".to_owned()),
        }
    }

    fn check_user(&self) -> Outcome {
        if self.uid == 0 {
            Outcome::fail(
                "running as root",
                "run hoster (and doctor) as the unprivileged hoster user, e.g. \
                 `sudo machinectl shell hoster@` (ADR-0023)",
            )
        } else {
            Outcome::Pass(format!("uid {}", self.uid))
        }
    }

    fn check_config(config: Result<&Config, &ConfigError>) -> Outcome {
        match config {
            Ok(config) => Outcome::Pass(format!(
                "admin host {}, state dir {}",
                config.admin_host,
                config.state_dir.display()
            )),
            Err(error) => Outcome::fail(
                error.to_string(),
                "fix hoster.toml or the HOSTER_* environment (see deploy/hoster.example.toml)",
            ),
        }
    }

    /// The state directory exists, is owned by this user, is closed to everyone else (it holds
    /// private keys and the secret store) and accepts new files.
    fn check_state_dir(&self, dir: &Path) -> Outcome {
        let shown = dir.display();
        let create = format!("mkdir -p -m 700 {shown}");
        let metadata = match std::fs::metadata(dir) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Outcome::fail(format!("{shown} does not exist"), create);
            }
            Err(e) => return Outcome::fail(format!("{shown}: {e}"), create),
        };
        if !metadata.is_dir() {
            return Outcome::fail(
                format!("{shown} is not a directory"),
                format!("remove it, then {create}"),
            );
        }
        if metadata.uid() != self.uid {
            return Outcome::fail(
                format!(
                    "{shown} is owned by uid {}, not {}",
                    metadata.uid(),
                    self.uid
                ),
                format!("chown -R <hoster user>: {shown}"),
            );
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Outcome::fail(
                format!("{shown} has mode {mode:03o}; group and others must have no access"),
                format!("chmod 700 {shown}"),
            );
        }
        let probe = dir.join(".hoster-doctor-probe");
        let written = std::fs::File::create(&probe).and_then(|mut f| f.write_all(b"ok"));
        let _ = std::fs::remove_file(&probe);
        match written {
            Ok(()) => Outcome::Pass(format!("{shown} (mode {mode:03o}, writable)")),
            Err(e) => Outcome::fail(
                format!("cannot write to {shown}: {e}"),
                format!("chmod 700 {shown}"),
            ),
        }
    }

    /// The first podman socket that exists answers `GET /_ping` with 200. `configured` is
    /// `podman_socket` from hoster.toml; when set it is the only candidate, as for `hoster run`.
    fn check_podman_socket(&self, configured: Option<&Path>) -> Outcome {
        let hint = "systemctl --user enable --now podman.socket (as the hoster user)";
        let configured = configured.map(|path| vec![path.to_path_buf()]);
        let sockets = configured.as_deref().unwrap_or(&self.sockets);
        let Some(socket) = sockets.iter().find(|s| s.exists()) else {
            let tried: Vec<String> = sockets.iter().map(|s| s.display().to_string()).collect();
            return Outcome::fail(format!("no socket at {}", tried.join(" or ")), hint);
        };
        match Ping::send(socket) {
            Ok(()) => Outcome::Pass(format!("{} answers /_ping", socket.display())),
            Err(e) => Outcome::fail(format!("{}: /_ping failed: {e}", socket.display()), hint),
        }
    }

    fn check_unprivileged_port_start(&self) -> Outcome {
        let hint = "echo 'net.ipv4.ip_unprivileged_port_start=80' | sudo tee \
                    /etc/sysctl.d/50-hoster.conf && sudo sysctl --system (ADR-0023)";
        let lowest = PUBLIC_PORTS[0];
        match self.sysctl("net/ipv4/ip_unprivileged_port_start") {
            Ok(value) => match value.parse::<u32>() {
                Ok(start) if start <= u32::from(lowest) => {
                    Outcome::Pass(format!("net.ipv4.ip_unprivileged_port_start = {start}"))
                }
                Ok(start) => Outcome::fail(
                    format!("net.ipv4.ip_unprivileged_port_start = {start}, above {lowest}"),
                    hint,
                ),
                Err(_) => Outcome::fail(format!("unexpected sysctl value {value:?}"), hint),
            },
            Err(e) => Outcome::fail(
                format!("cannot read net.ipv4.ip_unprivileged_port_start: {e}"),
                hint,
            ),
        }
    }

    /// Binds each public port on `[::]`, as the proxy does (architecture §7.1), and closes it
    /// again at once.
    fn check_bind_ports(&self) -> Outcome {
        let mut in_use = Vec::new();
        for &port in &self.ports {
            match TcpListener::bind((Ipv6Addr::UNSPECIFIED, port)) {
                Ok(listener) => drop(listener),
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => in_use.push(port),
                Err(e) => {
                    let hint = if e.kind() == io::ErrorKind::PermissionDenied {
                        "lower net.ipv4.ip_unprivileged_port_start to 80 (ADR-0023)"
                    } else {
                        "hoster listens on [::] (dual-stack); make sure IPv6 is not disabled"
                    };
                    return Outcome::fail(format!("cannot bind [::]:{port}: {e}"), hint);
                }
            }
        }
        let ports = self
            .ports
            .iter()
            .map(|p| format!(":{p}"))
            .collect::<Vec<_>>();
        if in_use.is_empty() {
            Outcome::Pass(format!("bound {}", ports.join(" and ")))
        } else {
            let busy = in_use.iter().map(|p| format!(":{p}")).collect::<Vec<_>>();
            Outcome::Warn {
                detail: format!("{} already in use", busy.join(" and ")),
                hint: "fine if hoster is running; otherwise stop whatever holds the port \
                       (`ss -ltnp`)"
                    .to_owned(),
            }
        }
    }

    fn check_linger(&self) -> Outcome {
        let hint = "sudo loginctl enable-linger <hoster user> (ADR-0023)";
        let uid = self.uid.to_string();
        match self.commands.stdout(
            "loginctl",
            &["show-user", &uid, "--property=Linger", "--value"],
        ) {
            Ok(value) if value == "yes" => Outcome::Pass("enabled".to_owned()),
            Ok(value) => Outcome::fail(format!("Linger={value}"), hint),
            Err(e) => Outcome::fail(format!("loginctl: {e}"), hint),
        }
    }

    fn check_podman_service(&self) -> Outcome {
        let hint = "systemctl --user enable --now podman.socket (as the hoster user)";
        let query = |verb| {
            self.commands
                .stdout("systemctl", &["--user", verb, "podman.socket"])
        };
        match (query("is-enabled"), query("is-active")) {
            (Ok(enabled), Ok(active)) if enabled == "enabled" && active == "active" => {
                Outcome::Pass("podman.socket enabled and active".to_owned())
            }
            (Ok(enabled), Ok(active)) => {
                Outcome::fail(format!("podman.socket is {enabled} and {active}"), hint)
            }
            (Err(e), _) | (_, Err(e)) => Outcome::fail(
                format!("systemctl --user: {e}"),
                "run doctor in a login session of the hoster user (XDG_RUNTIME_DIR set), \
                 e.g. `sudo machinectl shell hoster@`",
            ),
        }
    }

    /// The user manager delegates the cgroup controllers that resource limits will need
    /// (ADR-0023, ADR-0038). Advisory: v1 sets no limits.
    fn check_cgroup_delegation(&self) -> Outcome {
        let uid = self.uid;
        let path = self.cgroup_root.join(format!(
            "user.slice/user-{uid}.slice/user@{uid}.service/cgroup.controllers"
        ));
        let controllers = match std::fs::read_to_string(&path) {
            Ok(controllers) => controllers,
            Err(e) => {
                return Outcome::fail(
                    format!("cannot read {}: {e}", path.display()),
                    "needs cgroup v2 and a running user manager (linger enabled)",
                );
            }
        };
        let delegated: Vec<&str> = controllers.split_whitespace().collect();
        let missing: Vec<&str> = DELEGATED_CONTROLLERS
            .into_iter()
            .filter(|c| !delegated.contains(c))
            .collect();
        if missing.is_empty() {
            Outcome::Pass(format!("delegated: {}", delegated.join(" ")))
        } else {
            Outcome::fail(
                format!("not delegated: {}", missing.join(" ")),
                "add `[Service]` `Delegate=cpu cpuset io memory pids` in \
                 /etc/systemd/system/user@.service.d/delegate.conf, then \
                 `sudo systemctl daemon-reload` and restart user@<uid>.service",
            )
        }
    }

    /// systemd ≥ 256 can keep the secret-store key as a user-scoped encrypted credential
    /// (ADR-0029); older versions need a 0600 key file.
    fn check_user_credentials(&self) -> Outcome {
        let hint = "keep the secret-store key in a 0600 file instead (ADR-0029)";
        match self.commands.stdout("systemctl", &["--version"]) {
            Ok(output) => match SystemdVersion::from_version_output(&output) {
                Some(SystemdVersion(v)) if v >= USER_CREDENTIALS_SYSTEMD => Outcome::Pass(format!(
                    "systemd {v} supports user-scoped encrypted credentials"
                )),
                Some(SystemdVersion(v)) => Outcome::fail(
                    format!(
                        "systemd {v} is older than {USER_CREDENTIALS_SYSTEMD}; no user-scoped \
                         encrypted credentials"
                    ),
                    hint,
                ),
                None => Outcome::fail(format!("cannot parse {output:?}"), hint),
            },
            Err(e) => Outcome::fail(format!("systemctl --version: {e}"), hint),
        }
    }

    /// The unit's file-system sandboxing (`ProtectSystem=`, `ProtectHome=`, `ReadWritePaths=`,
    /// `PrivateTmp=`, …) needs a mount namespace, which a user manager can only set up inside a
    /// user namespace where it holds capabilities. Ubuntu ≥ 23.10 lets unprivileged processes
    /// create user namespaces but, with `kernel.apparmor_restrict_unprivileged_userns = 1`,
    /// gives them no capabilities there, so systemd silently skips those directives. hoster
    /// still runs (seccomp filters and `NoNewPrivileges=` apply); this only reports it.
    fn check_unit_sandbox(&self) -> Outcome {
        const KEY: &str = "kernel/apparmor_restrict_unprivileged_userns";
        match self.sysctl(KEY) {
            Ok(value) if value == "1" => Outcome::fail(
                "kernel.apparmor_restrict_unprivileged_userns = 1: the unit's file-system \
                 sandboxing (ProtectSystem, ProtectHome, ReadWritePaths, PrivateTmp, \
                 ProtectKernelTunables, ProtectControlGroups) is not applied; seccomp filters \
                 and NoNewPrivileges still are",
                "see https://docs.hoster.lhelge.se/wiki/install/#hardening (OQ-36)",
            ),
            Ok(value) => Outcome::Pass(format!(
                "kernel.apparmor_restrict_unprivileged_userns = {value}; the unit's sandboxing \
                 applies"
            )),
            // Not an AppArmor kernel with that restriction (Debian, Fedora, older Ubuntu).
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Outcome::Pass("no AppArmor user-namespace restriction".to_owned())
            }
            Err(e) => Outcome::fail(
                format!("cannot read kernel.apparmor_restrict_unprivileged_userns: {e}"),
                "see https://docs.hoster.lhelge.se/wiki/install/#hardening (OQ-36)",
            ),
        }
    }

    /// The upstream port range must not overlap the kernel's ephemeral range, or outgoing
    /// connections can grab ports hoster wants to hand out (ADR-0026).
    fn check_port_range(&self, ports: PortRange) -> Outcome {
        let hint = "check `sysctl net.ipv4.ip_local_port_range`";
        let local = match self.sysctl("net/ipv4/ip_local_port_range") {
            Ok(value) => match EphemeralPorts::parse(&value) {
                Some(local) => local,
                None => return Outcome::fail(format!("unexpected sysctl value {value:?}"), hint),
            },
            Err(e) => {
                return Outcome::fail(
                    format!("cannot read net.ipv4.ip_local_port_range: {e}"),
                    hint,
                );
            }
        };
        let (start, end) = (*local.0.start(), *local.0.end());
        if ports.first() <= end && start <= ports.last() {
            Outcome::fail(
                format!(
                    "upstream_ports {ports} overlaps net.ipv4.ip_local_port_range {start}-{end}"
                ),
                "move upstream_ports in hoster.toml outside the ephemeral range",
            )
        } else {
            Outcome::Pass(format!(
                "upstream_ports {ports} is clear of ephemeral ports {start}-{end}"
            ))
        }
    }

    /// OIDC token expiry checks on the deploy webhook need a correct clock (ADR-0047).
    fn check_clock(&self) -> Outcome {
        let hint = "enable time sync: sudo timedatectl set-ntp true";
        match self.commands.stdout(
            "timedatectl",
            &["show", "--property=NTPSynchronized", "--value"],
        ) {
            Ok(value) if value == "yes" => Outcome::Pass("NTP-synchronized".to_owned()),
            Ok(value) => Outcome::fail(format!("NTPSynchronized={value}"), hint),
            Err(e) => Outcome::fail(format!("timedatectl: {e}"), hint),
        }
    }

    /// The running binary is `current` in the versioned layout (ADR-0052), which updates and
    /// rollbacks from the UI and CLI need.
    fn check_binary_layout(&self) -> Outcome {
        let Some(installation) = &self.installation else {
            return Outcome::fail(
                "$HOME is not set, so ~/.local/lib/hoster cannot be located",
                "run as the hoster user with its login environment",
            );
        };
        let root = installation.root().display();
        let Some(exe) = &self.exe else {
            return Outcome::fail("cannot locate the running binary", "");
        };
        let Some(running) = installation.version_of(exe) else {
            return Outcome::fail(
                format!("{} is not inside {root}/versions", exe.display()),
                "fine for a manual install, but self-update and rollback need the versioned \
                 layout; install with install.sh (https://docs.hoster.lhelge.se/wiki/install/)",
            );
        };
        match installation.current() {
            Ok(Some(current)) if current == running => {
                Outcome::Pass(format!("running {running}, the current version in {root}"))
            }
            Ok(Some(current)) => Outcome::fail(
                format!("running {running}, but {root}/current is {current}"),
                "systemctl --user restart hoster",
            ),
            Ok(None) => Outcome::fail(
                format!("running {running}, but {root}/current names no installed version"),
                format!("ln -sfn versions/{running} {root}/current"),
            ),
            Err(e) => Outcome::fail(e.to_string(), ""),
        }
    }

    /// Reads a sysctl below `/proc/sys`, trimmed.
    fn sysctl(&self, key: &str) -> io::Result<String> {
        Ok(std::fs::read_to_string(self.proc_sys.join(key))?
            .trim()
            .to_owned())
    }
}

/// A plain HTTP/1.1 `GET /_ping` over the podman Unix socket.
struct Ping;

impl Ping {
    fn send(socket: &Path) -> io::Result<()> {
        let mut stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(PING_TIMEOUT))?;
        stream.set_write_timeout(Some(PING_TIMEOUT))?;
        stream.write_all(b"GET /_ping HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n")?;
        let mut response = Vec::new();
        // Read the status line; the rest of the response does not matter.
        let mut buf = [0u8; 256];
        while !response.contains(&b'\n') && response.len() < PING_MAX_STATUS_LINE {
            match stream.read(&mut buf)? {
                0 => break,
                n => response.extend_from_slice(&buf[..n]),
            }
        }
        let response = String::from_utf8_lossy(&response);
        let status = response.lines().next().unwrap_or_default();
        match status.split_whitespace().nth(1) {
            Some("200") if status.starts_with("HTTP/1.") => Ok(()),
            _ if status.is_empty() => Err(io::Error::other("empty response")),
            _ => Err(io::Error::other(format!("unexpected response {status:?}"))),
        }
    }
}

/// The major version from `systemctl --version` ("systemd 256 (256.5-1-arch)").
struct SystemdVersion(u32);

impl SystemdVersion {
    fn from_version_output(output: &str) -> Option<Self> {
        let mut words = output.lines().next()?.split_whitespace();
        (words.next()? == "systemd")
            .then(|| words.next()?.parse().ok().map(Self))
            .flatten()
    }
}

/// `net.ipv4.ip_local_port_range`: two numbers separated by whitespace.
struct EphemeralPorts(RangeInclusive<u16>);

impl EphemeralPorts {
    fn parse(value: &str) -> Option<Self> {
        let mut numbers = value.split_whitespace().map(str::parse::<u16>);
        match (numbers.next(), numbers.next(), numbers.next()) {
            (Some(Ok(start)), Some(Ok(end)), None) if start <= end => Some(Self(start..=end)),
            _ => None,
        }
    }
}

/// What one check found, before it is classed as required or advisory.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Pass(String),
    Warn { detail: String, hint: String },
    Fail { detail: String, hint: String },
    Skip(String),
}

impl Outcome {
    fn fail(detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::Fail {
            detail: detail.into(),
            hint: hint.into(),
        }
    }
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Pass => " ok ",
            Status::Warn => "warn",
            Status::Fail => "FAIL",
            Status::Skip => "skip",
        })
    }
}

/// One check in a [`Report`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    /// Whether a failure makes the whole report fail; advisory checks only warn.
    pub required: bool,
    pub detail: String,
    /// How to fix it; present for warnings and failures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn required(name: &'static str, outcome: Outcome) -> Self {
        Self::new(name, true, outcome)
    }

    /// A failed advisory check is reported as a warning.
    fn advisory(name: &'static str, outcome: Outcome) -> Self {
        let outcome = match outcome {
            Outcome::Fail { detail, hint } => Outcome::Warn { detail, hint },
            other => other,
        };
        Self::new(name, false, outcome)
    }

    fn new(name: &'static str, required: bool, outcome: Outcome) -> Self {
        let hint = |hint: String| (!hint.is_empty()).then_some(hint);
        let (status, detail, hint) = match outcome {
            Outcome::Pass(detail) => (Status::Pass, detail, None),
            Outcome::Skip(detail) => (Status::Skip, detail, None),
            Outcome::Warn { detail, hint: h } => (Status::Warn, detail, hint(h)),
            Outcome::Fail { detail, hint: h } => (Status::Fail, detail, hint(h)),
        };
        Self {
            name,
            status,
            required,
            detail,
            hint,
        }
    }
}

/// Every check's result, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    checks: Vec<Check>,
}

impl Report {
    /// Whether every required check passed (warnings and skips do not count).
    pub fn ok(&self) -> bool {
        self.failures() == 0
    }

    /// Number of failed checks.
    pub fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count()
    }

    #[cfg(test)]
    fn checks(&self) -> &[Check] {
        &self.checks
    }
}

impl Serialize for Report {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("Report", 2)?;
        s.serialize_field("ok", &self.ok())?;
        s.serialize_field("checks", &self.checks)?;
        s.end()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self.checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
        for check in &self.checks {
            writeln!(
                f,
                "[{}] {:width$}  {}",
                check.status, check.name, check.detail
            )?;
            if let Some(hint) = &check.hint {
                writeln!(f, "       {:width$}  hint: {hint}", "")?;
            }
        }
        if self.ok() {
            write!(f, "all required checks passed")
        } else {
            write!(f, "{} required check(s) failed", self.failures())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::net::UnixListener;

    /// Canned command output keyed by "program arg arg…"; anything else is "not found".
    #[derive(Default)]
    struct FakeCommands(HashMap<String, String>);

    impl FakeCommands {
        fn healthy() -> Self {
            Self::default()
                .with("loginctl show-user 1000 --property=Linger --value", "yes")
                .with("systemctl --user is-enabled podman.socket", "enabled")
                .with("systemctl --user is-active podman.socket", "active")
                .with("systemctl --version", "systemd 256 (256.5-1-arch)\n+PAM")
                .with("timedatectl show --property=NTPSynchronized --value", "yes")
        }

        fn with(mut self, command: &str, output: &str) -> Self {
            self.0.insert(command.to_owned(), output.to_owned());
            self
        }
    }

    impl Commands for FakeCommands {
        fn stdout(&self, program: &str, args: &[&str]) -> io::Result<String> {
            let key = std::iter::once(program)
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            self.0
                .get(&key)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "not found"))
        }
    }

    /// A temporary host: a fake /proc/sys, no podman socket, port 0 to bind.
    struct Fixture {
        dir: tempfile::TempDir,
        doctor: Doctor,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_commands(FakeCommands::healthy())
        }

        fn with_commands(commands: FakeCommands) -> Self {
            let dir = tempfile::tempdir().unwrap();
            // hoster v0.1.0 installed and current, running through `current`.
            let installation = Installation::new(dir.path().join("lib/hoster"));
            let download = dir.path().join("hoster-download");
            std::fs::write(&download, "binary").unwrap();
            let version = "v0.1.0".parse().unwrap();
            installation.install(&version, &download).unwrap();
            installation.switch(&version).unwrap();
            let doctor = Doctor {
                uid: 1000,
                proc_sys: dir.path().join("sys"),
                cgroup_root: dir.path().join("cgroup"),
                sockets: vec![dir.path().join("run/podman/podman.sock")],
                ports: vec![0],
                exe: Some(installation.current_link().join("hoster")),
                installation: Some(installation),
                commands: Box::new(commands),
            };
            let fixture = Self { dir, doctor };
            fixture.sysctl("net/ipv4/ip_unprivileged_port_start", "80\n");
            fixture.sysctl("net/ipv4/ip_local_port_range", "32768\t60999\n");
            fixture
        }

        fn sysctl(&self, key: &str, value: &str) {
            let path = self.doctor.proc_sys.join(key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, value).unwrap();
        }

        /// The controllers the current uid's user manager has delegated.
        fn cgroup_controllers(&self, controllers: &str) {
            let uid = self.doctor.uid;
            let dir = self
                .doctor
                .cgroup_root
                .join(format!("user.slice/user-{uid}.slice/user@{uid}.service"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("cgroup.controllers"), controllers).unwrap();
        }

        /// A state directory with `mode`, owned by the test user, whose uid the doctor adopts.
        fn state_dir(&mut self, mode: u32) -> PathBuf {
            let path = self.dir.path().join("state");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            self.doctor.uid = std::fs::metadata(&path).unwrap().uid();
            path
        }

        /// A fake podman socket answering every connection with `response`.
        fn podman(&self, socket: &Path, response: &'static str) {
            std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
            let listener = UnixListener::bind(socket).unwrap();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    let mut request = [0u8; 512];
                    let n = stream.read(&mut request).unwrap();
                    assert!(request[..n].starts_with(b"GET /_ping HTTP/1.1\r\n"));
                    stream.write_all(response.as_bytes()).unwrap();
                }
            });
        }
    }

    fn config(state_dir: PathBuf, ports: RangeInclusive<u16>) -> Config {
        Config {
            secret_key_file: state_dir.with_file_name("secret.key"),
            state_dir,
            admin_host: "hoster.example.com".parse().unwrap(),
            admin_github: "octocat".parse().unwrap(),
            podman_socket: None,
            acme: Default::default(),
            upstream_ports: PortRange::try_from(ports).unwrap(),
            proxy: Default::default(),
            poller: Default::default(),
            webhook: Default::default(),
            web: Default::default(),
            log: Default::default(),
            credentials: Default::default(),
            source: Default::default(),
        }
    }

    fn status(outcome: Outcome) -> &'static str {
        match outcome {
            Outcome::Pass(_) => "pass",
            Outcome::Warn { .. } => "warn",
            Outcome::Fail { .. } => "fail",
            Outcome::Skip(_) => "skip",
        }
    }

    const OK_PING: &str = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";

    #[test]
    fn root_is_rejected() {
        let mut fx = Fixture::new();
        assert_eq!(status(fx.doctor.check_user()), "pass");
        fx.doctor.uid = 0;
        assert_eq!(status(fx.doctor.check_user()), "fail");
    }

    #[test]
    fn state_dir_must_exist_be_private_and_writable() {
        let mut fx = Fixture::new();
        let missing = fx.dir.path().join("nope");
        let Outcome::Fail { detail, hint } = fx.doctor.check_state_dir(&missing) else {
            panic!("missing dir must fail");
        };
        assert!(detail.contains("does not exist"));
        assert!(hint.contains("mkdir -p -m 700"));

        let dir = fx.state_dir(0o700);
        assert_eq!(status(fx.doctor.check_state_dir(&dir)), "pass");
        assert!(!dir.join(".hoster-doctor-probe").exists());

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let Outcome::Fail { hint, .. } = fx.doctor.check_state_dir(&dir) else {
            panic!("0755 must fail");
        };
        assert!(hint.contains("chmod 700"));

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        fx.doctor.uid += 1;
        assert_eq!(status(fx.doctor.check_state_dir(&dir)), "fail");
    }

    #[test]
    fn state_dir_that_is_a_file_fails() {
        let fx = Fixture::new();
        let file = fx.dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(status(fx.doctor.check_state_dir(&file)), "fail");
    }

    #[test]
    fn unwritable_state_dir_fails() {
        let mut fx = Fixture::new();
        let dir = fx.state_dir(0o500);
        // Root ignores permission bits, so only assert when the probe really is refused.
        if std::fs::File::create(dir.join("x")).is_err() {
            assert_eq!(status(fx.doctor.check_state_dir(&dir)), "fail");
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn podman_socket_answers_ping() {
        let fx = Fixture::new();
        let Outcome::Fail { detail, .. } = fx.doctor.check_podman_socket(None) else {
            panic!("no socket must fail");
        };
        assert!(detail.contains("no socket at"));

        fx.podman(&fx.doctor.sockets[0], OK_PING);
        assert_eq!(status(fx.doctor.check_podman_socket(None)), "pass");
    }

    #[test]
    fn podman_socket_later_candidate_is_used() {
        let mut fx = Fixture::new();
        let second = fx.dir.path().join("fallback/podman/podman.sock");
        fx.doctor.sockets.push(second.clone());
        fx.podman(&second, OK_PING);
        let Outcome::Pass(detail) = fx.doctor.check_podman_socket(None) else {
            panic!("second candidate must pass");
        };
        assert!(detail.contains("fallback"));
    }

    #[test]
    fn configured_podman_socket_replaces_discovery() {
        let fx = Fixture::new();
        fx.podman(&fx.doctor.sockets[0], OK_PING);
        let configured = fx.dir.path().join("configured/podman.sock");
        let Outcome::Fail { detail, .. } = fx.doctor.check_podman_socket(Some(&configured)) else {
            panic!("a missing configured socket must fail even though discovery would pass");
        };
        assert!(detail.contains("configured/podman.sock"), "{detail}");

        fx.podman(&configured, OK_PING);
        let Outcome::Pass(detail) = fx.doctor.check_podman_socket(Some(&configured)) else {
            panic!("configured socket must pass");
        };
        assert!(detail.contains("configured/podman.sock"), "{detail}");
    }

    #[test]
    fn podman_socket_error_status_fails() {
        let fx = Fixture::new();
        fx.podman(
            &fx.doctor.sockets[0],
            "HTTP/1.1 500 Internal Server Error\r\n\r\n",
        );
        let Outcome::Fail { detail, .. } = fx.doctor.check_podman_socket(None) else {
            panic!("500 must fail");
        };
        assert!(detail.contains("500"));
    }

    #[test]
    fn unprivileged_port_start_must_be_at_most_80() {
        let fx = Fixture::new();
        assert_eq!(status(fx.doctor.check_unprivileged_port_start()), "pass");
        fx.sysctl("net/ipv4/ip_unprivileged_port_start", "1024\n");
        let Outcome::Fail { hint, .. } = fx.doctor.check_unprivileged_port_start() else {
            panic!("1024 must fail");
        };
        assert!(hint.contains("ip_unprivileged_port_start=80"));
        fx.sysctl("net/ipv4/ip_unprivileged_port_start", "garbage");
        assert_eq!(status(fx.doctor.check_unprivileged_port_start()), "fail");
        std::fs::remove_file(
            fx.doctor
                .proc_sys
                .join("net/ipv4/ip_unprivileged_port_start"),
        )
        .unwrap();
        assert_eq!(status(fx.doctor.check_unprivileged_port_start()), "fail");
    }

    #[test]
    fn ports_are_bound_and_released() {
        let mut fx = Fixture::new();
        assert_eq!(status(fx.doctor.check_bind_ports()), "pass");

        let held = TcpListener::bind((Ipv6Addr::UNSPECIFIED, 0)).unwrap();
        fx.doctor.ports = vec![held.local_addr().unwrap().port()];
        assert_eq!(status(fx.doctor.check_bind_ports()), "warn");
        drop(held);
        // Released by doctor itself: binding again still works.
        assert_eq!(status(fx.doctor.check_bind_ports()), "pass");
    }

    #[test]
    fn linger_must_be_enabled() {
        let fx = Fixture::new();
        assert_eq!(status(fx.doctor.check_linger()), "pass");
        let fx = Fixture::with_commands(
            FakeCommands::healthy().with("loginctl show-user 1000 --property=Linger --value", "no"),
        );
        let Outcome::Fail { detail, hint } = fx.doctor.check_linger() else {
            panic!("no linger must fail");
        };
        assert_eq!(detail, "Linger=no");
        assert!(hint.contains("enable-linger"));
        let fx = Fixture::with_commands(FakeCommands::default());
        assert_eq!(status(fx.doctor.check_linger()), "fail");
    }

    #[test]
    fn podman_socket_unit_must_be_enabled_and_active() {
        let fx = Fixture::new();
        assert_eq!(status(fx.doctor.check_podman_service()), "pass");
        let fx = Fixture::with_commands(
            FakeCommands::healthy().with("systemctl --user is-active podman.socket", "inactive"),
        );
        let Outcome::Fail { detail, .. } = fx.doctor.check_podman_service() else {
            panic!("inactive must fail");
        };
        assert_eq!(detail, "podman.socket is enabled and inactive");
        let fx = Fixture::with_commands(
            FakeCommands::healthy().with("systemctl --user is-enabled podman.socket", "disabled"),
        );
        assert_eq!(status(fx.doctor.check_podman_service()), "fail");
        let fx = Fixture::with_commands(FakeCommands::default());
        assert_eq!(status(fx.doctor.check_podman_service()), "fail");
    }

    #[test]
    fn cgroup_controllers_must_be_delegated() {
        let fx = Fixture::new();
        let Outcome::Fail { detail, .. } = fx.doctor.check_cgroup_delegation() else {
            panic!("missing cgroup.controllers must be reported");
        };
        assert!(
            detail.contains("user@1000.service/cgroup.controllers"),
            "{detail}"
        );
        // systemd's default delegation lacks cpu.
        fx.cgroup_controllers("memory pids\n");
        let Outcome::Fail { detail, hint } = fx.doctor.check_cgroup_delegation() else {
            panic!("missing cpu must be reported");
        };
        assert_eq!(detail, "not delegated: cpu");
        assert!(hint.contains("Delegate="));
        fx.cgroup_controllers("cpuset cpu io memory pids\n");
        assert_eq!(status(fx.doctor.check_cgroup_delegation()), "pass");
    }

    #[test]
    fn user_credentials_need_systemd_256() {
        let fx = Fixture::new();
        assert_eq!(status(fx.doctor.check_user_credentials()), "pass");
        let fx = Fixture::with_commands(
            FakeCommands::healthy().with("systemctl --version", "systemd 255 (255.4-1ubuntu8)"),
        );
        assert_eq!(status(fx.doctor.check_user_credentials()), "fail");
        let fx = Fixture::with_commands(
            FakeCommands::healthy().with("systemctl --version", "something else"),
        );
        assert_eq!(status(fx.doctor.check_user_credentials()), "fail");
    }

    #[test]
    fn port_range_must_avoid_ephemeral_ports() {
        let fx = Fixture::new();
        let default = PortRange::default();
        assert_eq!(status(fx.doctor.check_port_range(default)), "pass");
        let overlapping = PortRange::try_from(30000..=40000).unwrap();
        let Outcome::Fail { detail, .. } = fx.doctor.check_port_range(overlapping) else {
            panic!("overlap must be reported");
        };
        assert!(detail.contains("32768-60999"));
        // Touching at one end is an overlap too.
        let edge = PortRange::try_from(60999..=61100).unwrap();
        assert_eq!(status(fx.doctor.check_port_range(edge)), "fail");
        let above = PortRange::try_from(61000..=61100).unwrap();
        assert_eq!(status(fx.doctor.check_port_range(above)), "pass");
        fx.sysctl("net/ipv4/ip_local_port_range", "nonsense");
        assert_eq!(status(fx.doctor.check_port_range(default)), "fail");
    }

    #[test]
    fn clock_must_be_ntp_synchronized() {
        let fx = Fixture::new();
        assert_eq!(status(fx.doctor.check_clock()), "pass");
        let fx = Fixture::with_commands(
            FakeCommands::healthy()
                .with("timedatectl show --property=NTPSynchronized --value", "no"),
        );
        assert_eq!(status(fx.doctor.check_clock()), "fail");
    }

    #[test]
    fn systemd_version_parsing() {
        let parse = |s| SystemdVersion::from_version_output(s).map(|v| v.0);
        assert_eq!(parse("systemd 262 (262.1-1-arch)\n+PAM +AUDIT"), Some(262));
        assert_eq!(parse("systemd 249"), Some(249));
        assert_eq!(parse("not systemd"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn binary_outside_the_layout_only_warns() {
        let mut fx = Fixture::new();
        let Outcome::Pass(detail) = fx.doctor.check_binary_layout() else {
            panic!("the fixture runs from current");
        };
        assert!(detail.contains("running v0.1.0"), "{detail}");

        // A manual install elsewhere: a warning in the report, not a failure.
        fx.doctor.exe = Some(fx.dir.path().join("hoster-download"));
        let Outcome::Fail { detail, hint } = fx.doctor.check_binary_layout() else {
            panic!("a binary outside the layout must be flagged");
        };
        assert!(detail.contains("is not inside"), "{detail}");
        assert!(hint.contains("install.sh"), "{hint}");
        let report = fx.doctor.diagnose(&Err(ConfigError::NoConfigDir));
        let check = report
            .checks()
            .iter()
            .find(|c| c.name == "binary_layout")
            .unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(!check.required);

        // Running a version that is not current (switched, not yet restarted).
        let installation = fx.doctor.installation.clone().unwrap();
        let download = fx.dir.path().join("hoster-download");
        let newer = "v0.2.0".parse().unwrap();
        installation.install(&newer, &download).unwrap();
        installation.switch(&newer).unwrap();
        fx.doctor.exe = Some(installation.binary(&"v0.1.0".parse().unwrap()));
        let Outcome::Fail { detail, hint } = fx.doctor.check_binary_layout() else {
            panic!("a stale binary must be flagged");
        };
        assert!(detail.contains("current is v0.2.0"), "{detail}");
        assert!(hint.contains("restart"), "{hint}");

        fx.doctor.installation = None;
        assert_eq!(status(fx.doctor.check_binary_layout()), "fail");
    }

    #[test]
    fn healthy_host_passes() {
        let mut fx = Fixture::new();
        let state = fx.state_dir(0o700);
        let uid = fx.doctor.uid.to_string();
        fx.doctor.commands = Box::new(FakeCommands::healthy().with(
            &format!("loginctl show-user {uid} --property=Linger --value"),
            "yes",
        ));
        fx.cgroup_controllers("cpu memory pids");
        fx.podman(&fx.doctor.sockets[0], OK_PING);
        let report = fx.doctor.diagnose(&Ok(config(state, 20000..=29999)));
        if fx.doctor.uid == 0 {
            return; // Running the suite as root: the user check rightly fails.
        }
        assert!(report.ok(), "{report}");
        assert!(report.checks().iter().all(|c| c.status == Status::Pass));
        assert!(report.to_string().ends_with("all required checks passed"));
    }

    #[test]
    fn apparmor_userns_restriction_is_reported() {
        let fx = Fixture::new();
        // No such sysctl: not an AppArmor kernel with the restriction.
        assert_eq!(status(fx.doctor.check_unit_sandbox()), "pass");
        fx.sysctl("kernel/apparmor_restrict_unprivileged_userns", "0\n");
        assert_eq!(status(fx.doctor.check_unit_sandbox()), "pass");
        fx.sysctl("kernel/apparmor_restrict_unprivileged_userns", "1\n");
        let Outcome::Fail { detail, hint } = fx.doctor.check_unit_sandbox() else {
            panic!("the restriction must be reported");
        };
        assert!(detail.contains("ProtectSystem"), "{detail}");
        assert!(hint.contains("OQ-36"), "{hint}");
        let report = fx
            .doctor
            .diagnose(&Err(ConfigError::NotFound(PathBuf::from("/nope"))));
        let check = report
            .checks()
            .iter()
            .find(|c| c.name == "unit_sandbox")
            .unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(!check.required);
    }

    #[test]
    fn failures_are_counted_and_advisory_checks_only_warn() {
        let fx = Fixture::with_commands(FakeCommands::default());
        let error = ConfigError::NotFound(PathBuf::from("/nope/hoster.toml"));
        let report = fx.doctor.diagnose(&Err(error));
        let by_name = |name| {
            report
                .checks()
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("check {name}"))
        };
        assert_eq!(by_name("config").status, Status::Fail);
        assert_eq!(by_name("state_dir").status, Status::Skip);
        assert_eq!(by_name("port_range").status, Status::Skip);
        assert!(by_name("port_range").required);
        assert_eq!(by_name("cgroup_delegation").status, Status::Warn);
        assert_eq!(by_name("user_credentials").status, Status::Warn);
        assert!(!by_name("user_credentials").required);
        assert_eq!(by_name("linger").status, Status::Fail);
        assert!(!report.ok());
        // config, podman_socket, linger, podman_service, clock.
        assert_eq!(report.failures(), 5);
        let text = report.to_string();
        assert!(text.contains("[FAIL] config"), "{text}");
        assert!(text.contains("hint: "), "{text}");
        assert!(text.ends_with("5 required check(s) failed"), "{text}");
    }

    #[test]
    fn report_serializes_to_json() {
        let report = Report {
            checks: vec![
                Check::required("a", Outcome::Pass("fine".to_owned())),
                Check::advisory("b", Outcome::fail("meh", "do x")),
            ],
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "ok": true,
                "checks": [
                    {"name": "a", "status": "pass", "required": true, "detail": "fine"},
                    {"name": "b", "status": "warn", "required": false, "detail": "meh", "hint": "do x"},
                ]
            })
        );
    }
}
