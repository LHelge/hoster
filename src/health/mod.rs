//! Health checks of new containers (ADR-0038, as amended by ADR-0051 #3 and ADR-0056 #4).
//!
//! hoster probes a container **from the host**, on the `127.0.0.1` port allocated for one of its
//! route ports, so distroless images need no curl, or, for a declared `health.exec`, by running
//! a command inside it through podman's exec API. The public surface is small:
//!
//! - [`Check::for_service`] resolves a service's declared `health` table (or the defaults) and
//!   its allocated [`PortMap`] into a [`Check`]: an `http` probe, a `tcp` probe, an `exec` probe
//!   (paired with a TCP connect to the first route's port on a routed service), or, for a
//!   service without routes, "running and not restarted for 5 s". Its alternate `Display`
//!   (`{check:#}`) says which check it is and why.
//! - [`Check::run`] carries it out against one container, through [`Probes`] (the runtime, a
//!   [`Prober`] for the network and the [`ExecSlots`]), until it passes, its `start_period` runs
//!   out, or the container exits (checked with `inspect` before every probe and once more before
//!   a pass is reported). An exec check podman cannot run fails at once.
//! - Results go to a [`ProbeLog`]; [`DeploymentLog`] records them as deployment events, an exec's
//!   output tail with the deployment's secrets redacted.
//! - After a deploy, [`Check::live`] probes a live container once, through the same [`Probes`],
//!   and [`Liveness`] turns the results into a [`HealthStatus`] (see `liveness`).
//!
//! hoster turns podman's restart policy off (ADR-0028), so "running at every look" is "running
//! and not restarted".

use std::borrow::Cow;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

use crate::runtime::{ContainerId, ContainerRuntime, ContainerState, ExecOutcome, RuntimeError};
use crate::secrets::SecretValue;
use crate::stack::{DEFAULT_HEALTH_PATH, ExpectStatus, Probe, Service};
use crate::store::{DeploymentId, DeploymentState, PortMap, Store};

mod exec;
mod liveness;

pub use exec::ExecSlots;
use exec::{AtCap, CannotRun, Command, SlotError, Tail};

pub use liveness::{
    HealthChange, HealthStats, HealthStatus, Liveness, MonitorSettings, NetworkProber, Prober,
};

/// Time between two probes when `interval` is unset.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
/// How long one probe may take when `timeout` is unset.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long one exec probe may take when `timeout` is unset (ADR-0056).
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(5);
/// Consecutive passes required when `healthy_after` is unset.
pub const DEFAULT_HEALTHY_AFTER: u32 = 1;
/// How long a container gets to become healthy when `start_period` is unset.
pub const DEFAULT_START_PERIOD: Duration = Duration::from_secs(30);
/// A service without routes is healthy after running this long (ADR-0038).
pub const DEFAULT_SETTLE: Duration = Duration::from_secs(5);
/// How often the settle check inspects the container.
pub const DEFAULT_SETTLE_POLL: Duration = Duration::from_millis(500);

/// Longest HTTP status line the probe reads before giving up on the answer.
const MAX_STATUS_LINE: usize = 1024;

/// A resolved health check for the containers of one service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    kind: Kind,
    /// Time between probes (between inspections for [`Kind::Settle`]).
    interval: Duration,
    /// Per-probe timeout.
    timeout: Duration,
    /// Consecutive passes required.
    healthy_after: u32,
    /// Give up after this long; for [`Kind::Settle`], how long the container must stay up.
    start_period: Duration,
    /// Why this check applies.
    origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Http {
        addr: SocketAddr,
        container_port: u16,
        path: String,
        expect: ExpectStatus,
    },
    Tcp {
        addr: SocketAddr,
        container_port: u16,
    },
    /// `command` run in the container; exit 0 passes. On a routed service also a TCP connect
    /// to the first route's port (`connect`: its loopback address and container port).
    Exec {
        command: Vec<String>,
        connect: Option<(SocketAddr, u16)>,
    },
    /// Running and not restarted for `start_period`.
    Settle,
}

/// Why a service gets its check (ADR-0056 precedence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Its `health` table.
    Declared,
    /// `GET /healthz`, a routed service without `health` (ADR-0051).
    RoutedDefault,
    /// Running for 5 s, a service without routes or `health` (ADR-0038).
    UnroutedDefault,
}

/// What a check probes with: the runtime (inspections and exec checks), the [`Prober`] for HTTP
/// and TCP, and the [`ExecSlots`] that keep exec probes of one container apart.
pub struct Probes<'a, R> {
    runtime: &'a R,
    prober: &'a dyn Prober,
    execs: ExecSlots,
}

impl<'a, R: ContainerRuntime> Probes<'a, R> {
    /// Probes through `runtime` and `prober`, sharing the exec slots `execs`.
    pub fn new(runtime: &'a R, prober: &'a dyn Prober, execs: &ExecSlots) -> Self {
        Self {
            runtime,
            prober,
            execs: execs.clone(),
        }
    }
}

/// How a probe runs: for a deployment (an exec waits at the cap until the deadline and its
/// output goes to `log`) or for the monitor (an exec at the cap starts none, exit code only).
#[derive(Clone, Copy)]
enum Mode<'a> {
    Deploy {
        deadline: Instant,
        log: &'a dyn ProbeLog,
    },
    Live,
}

/// One probe's verdict, and for an exec check in a deployment the tail of its output.
struct Probed {
    /// `Ok` with what passed, `Err` with why it failed: the status, compared between probes
    /// and the only part that reaches errors, the monitor, SSE and the event log.
    result: Result<String, String>,
    /// The exec's output tail, redacted, for the deployment event that records the result
    /// (ADR-0056 #4: deployment events only).
    tail: Option<String>,
}

impl From<Result<String, String>> for Probed {
    fn from(result: Result<String, String>) -> Self {
        Self { result, tail: None }
    }
}

/// Why a container did not become healthy.
#[derive(Debug, Error)]
pub enum HealthError {
    #[error("health probe port {0} has no allocated loopback port")]
    NoLoopbackPort(u16),
    /// Podman could not execute the exec check's argv[0] (ADR-0056): no retry helps.
    #[error("`health.exec` cannot run: {0}")]
    CannotRun(String),
    /// An exec found the container stopping or stopped (compat inspect may still say running).
    #[error("container {0} is stopping or stopped before it became healthy")]
    Stopping(ContainerId),
    #[error("cannot inspect container {id}: {source}")]
    Inspect {
        id: ContainerId,
        #[source]
        source: RuntimeError,
    },
    #[error("container {name} {state} before it became healthy")]
    Exited { name: String, state: ContainerState },
    #[error(
        "health check {check} did not pass within {}: last result: {last}{}",
        humantime::format_duration(*.within),
        if *.default_path { DEFAULT_PATH_HINT } else { "" }
    )]
    Unhealthy {
        check: String,
        within: Duration,
        last: String,
        /// The probe used the default path (ADR-0051), so the message says how to change it.
        default_path: bool,
    },
}

const DEFAULT_PATH_HINT: &str = " (`/healthz` is the default health path for routed services; \
                                 set `health.http.path` to probe another path)";

impl Check {
    /// The check for a service's new container, whose route ports were allocated `ports`
    /// (container port → loopback port). A declared `health` table wins, `exec` included on any
    /// service (on a routed one paired with a TCP connect to the first route's port, ADR-0056);
    /// otherwise a routed service gets `GET /healthz` on its first route's port, expecting
    /// 2xx–3xx (ADR-0051), and a service without routes gets "running and not restarted for
    /// 5 s".
    pub fn for_service(service: &Service, ports: &PortMap) -> Result<Self, HealthError> {
        let loopback = |container_port: u16| {
            ports
                .get(&container_port)
                .map(|&port| SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
                .ok_or(HealthError::NoLoopbackPort(container_port))
        };
        let http = |path: &str, container_port: u16, expect: ExpectStatus| {
            Ok::<_, HealthError>(Kind::Http {
                addr: loopback(container_port)?,
                container_port,
                path: path.to_owned(),
                expect,
            })
        };
        let Some(health) = &service.health else {
            return Ok(match service.routes.first() {
                Some(route) => {
                    let mut check = Self::probe(http(
                        DEFAULT_HEALTH_PATH,
                        route.port,
                        ExpectStatus::default(),
                    )?);
                    check.origin = Origin::RoutedDefault;
                    check
                }
                None => Self::settle(DEFAULT_SETTLE, DEFAULT_SETTLE_POLL),
            });
        };
        let kind = match &health.probe {
            Probe::Http {
                path,
                port,
                expect_status,
            } => http(path, *port, *expect_status)?,
            Probe::Tcp { port } => Kind::Tcp {
                addr: loopback(*port)?,
                container_port: *port,
            },
            Probe::Exec { command } => Kind::Exec {
                command: command.clone(),
                connect: service
                    .routes
                    .first()
                    .map(|route| Ok::<_, HealthError>((loopback(route.port)?, route.port)))
                    .transpose()?,
            },
        };
        let mut check = Self::probe(kind);
        if matches!(check.kind, Kind::Exec { .. }) {
            check.timeout = DEFAULT_EXEC_TIMEOUT;
        }
        check.interval = health.interval.unwrap_or(check.interval);
        check.timeout = health.timeout.unwrap_or(check.timeout);
        check.healthy_after = health.healthy_after.unwrap_or(check.healthy_after);
        check.start_period = health.start_period.unwrap_or(check.start_period);
        Ok(check)
    }

    /// "Running and not restarted for `period`", inspecting every `poll`.
    pub fn settle(period: Duration, poll: Duration) -> Self {
        Self {
            kind: Kind::Settle,
            interval: poll,
            timeout: DEFAULT_TIMEOUT,
            healthy_after: 1,
            start_period: period,
            origin: Origin::UnroutedDefault,
        }
    }

    /// A declared probe of `kind` with the default timings.
    fn probe(kind: Kind) -> Self {
        Self {
            kind,
            interval: DEFAULT_INTERVAL,
            timeout: DEFAULT_TIMEOUT,
            healthy_after: DEFAULT_HEALTHY_AFTER,
            start_period: DEFAULT_START_PERIOD,
            origin: Origin::Declared,
        }
    }

    /// The TCP connect an exec check of a routed service needs as well, with its timings.
    fn connect_part(&self) -> Option<Self> {
        let Kind::Exec {
            connect: Some((addr, container_port)),
            ..
        } = &self.kind
        else {
            return None;
        };
        Some(Self {
            kind: Kind::Tcp {
                addr: *addr,
                container_port: *container_port,
            },
            ..self.clone()
        })
    }

    /// Overrides the time between probes (or inspections).
    #[cfg(test)]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Overrides how long the container gets (or, for the settle check, must stay up).
    #[cfg(test)]
    pub fn with_start_period(mut self, start_period: Duration) -> Self {
        self.start_period = start_period;
        self
    }

    /// The longest grace after a (re)start: the monitor's quiet time and the reconciler's
    /// hold on a running dependency end after min(`start_period`, this) (ADR-0056).
    pub const MAX_GRACE: Duration = Duration::from_secs(5 * 60);

    /// min(`start_period`, [`Self::MAX_GRACE`]): how long after a (re)start a failing check
    /// writes no event and still holds the services depending on it (ADR-0056 Decision 3).
    /// For the settle check that is how long the container must stay up.
    pub fn grace(&self) -> Duration {
        self.start_period.min(Self::MAX_GRACE)
    }

    /// The time between probes, as a deploy probes.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Consecutive passes required.
    pub fn healthy_after(&self) -> u32 {
        self.healthy_after
    }

    /// One liveness probe of `container`, known to be running: through the prober for an HTTP
    /// or TCP check, in the container for an exec check (which reports its exit code only, and
    /// "check stuck" without starting an exec while [`ExecSlots::MAX_ABANDONED`] earlier ones
    /// still run); the settle check has nothing to probe, running is passing. A check that
    /// cannot run is a failure here, with the reason.
    pub async fn live<R: ContainerRuntime>(
        &self,
        probes: &Probes<'_, R>,
        container: &ContainerId,
    ) -> Result<String, String> {
        match self.kind {
            Kind::Settle => Ok("running".to_owned()),
            _ => match self.probe_once(probes, container, Mode::Live).await {
                Ok(probed) => probed.result,
                Err(HealthError::Stopping(_)) => {
                    Err("the container is stopping or stopped".to_owned())
                }
                Err(err) => Err(err.to_string()),
            },
        }
    }

    /// Whether this is the "running and not restarted" check of a service without routes.
    pub fn is_settle(&self) -> bool {
        self.kind == Kind::Settle
    }

    /// Runs the check against `container` until it passes, `start_period` runs out or the
    /// container stops running; each probe goes through `probes` (with [`NetworkProber`] in
    /// production). Probe results, and the verdict, go to `log`; a result is logged when its
    /// status differs from the previous one (an exec's output tail is attached to that event,
    /// never compared), so a steady failure is one event. An exec check podman cannot run ends
    /// the check at once.
    pub async fn run<R: ContainerRuntime>(
        &self,
        probes: &Probes<'_, R>,
        container: &ContainerId,
        log: &dyn ProbeLog,
    ) -> Result<(), HealthError> {
        let runtime = probes.runtime;
        let started = Instant::now();
        let deadline = started + self.start_period;
        let mut streak = 0;
        let mut last: Option<Result<String, String>> = None;
        loop {
            if let Err(err) = Self::ensure_running(runtime, container).await {
                log.record(&err.to_string()).await;
                return Err(err);
            }
            if self.kind != Kind::Settle {
                let mode = Mode::Deploy { deadline, log };
                let Probed { result, tail } = match self.probe_once(probes, container, mode).await {
                    Ok(probed) => probed,
                    Err(err) => {
                        // An exec that found the container stopping: say how it ended if it has.
                        let err = match err {
                            HealthError::Stopping(_) => Self::ensure_running(runtime, container)
                                .await
                                .err()
                                .unwrap_or(err),
                            err => err,
                        };
                        log.record(&err.to_string()).await;
                        return Err(err);
                    }
                };
                streak = if result.is_ok() { streak + 1 } else { 0 };
                if streak >= self.healthy_after {
                    // A container that exited right after answering is not healthy.
                    if let Err(err) = Self::ensure_running(runtime, container).await {
                        log.record(&err.to_string()).await;
                        return Err(err);
                    }
                    log.record(&format!(
                        "health check {self} passed {streak} time(s) in a row after {}",
                        Elapsed(started.elapsed())
                    ))
                    .await;
                    return Ok(());
                }
                if last.as_ref() != Some(&result) {
                    let (Ok(text) | Err(text)) = &result;
                    let message = match tail {
                        Some(tail) => format!("health probe {self}: {text}, output {tail}"),
                        None => format!("health probe {self}: {text}"),
                    };
                    log.record(&message).await;
                }
                last = Some(result);
            }
            let now = Instant::now();
            if now >= deadline {
                return match last {
                    // Settle: up at every look for the whole period.
                    None => Ok(()),
                    Some(last) => {
                        let (Ok(last) | Err(last)) = last;
                        let err = HealthError::Unhealthy {
                            check: self.to_string(),
                            within: self.start_period,
                            last,
                            default_path: matches!(
                                &self.kind,
                                Kind::Http { path, .. } if path == DEFAULT_HEALTH_PATH
                            ),
                        };
                        log.record(&err.to_string()).await;
                        Err(err)
                    }
                };
            }
            tokio::time::sleep(self.interval.min(deadline - now)).await;
        }
    }

    async fn ensure_running<R: ContainerRuntime>(
        runtime: &R,
        container: &ContainerId,
    ) -> Result<(), HealthError> {
        let info = runtime
            .inspect(container)
            .await
            .map_err(|source| HealthError::Inspect {
                id: container.clone(),
                source,
            })?;
        if info.state.is_running() {
            Ok(())
        } else {
            Err(HealthError::Exited {
                name: info.name,
                state: info.state,
            })
        }
    }

    /// One probe of `container`, or `Err` when the check must stop (the exec cannot run, the
    /// container is stopping). HTTP and TCP go through the prober; an exec check runs in the
    /// container and, on a routed service, connects to the first route's port through the
    /// prober at the same time.
    async fn probe_once<R: ContainerRuntime>(
        &self,
        probes: &Probes<'_, R>,
        container: &ContainerId,
        mode: Mode<'_>,
    ) -> Result<Probed, HealthError> {
        if !matches!(self.kind, Kind::Exec { .. }) {
            return Ok(probes.prober.probe(self).await.into());
        }
        let connect = async {
            match self.connect_part() {
                Some(tcp) => Some(probes.prober.probe(&tcp).await),
                None => None,
            }
        };
        let (exec, connect) = tokio::join!(self.exec(probes, container, mode), connect);
        let Probed { result, tail } = exec?;
        let result = match connect {
            None => result,
            Some(connect) => match (result, connect) {
                (Ok(exec), Ok(connect)) => Ok(format!("{exec}; TCP: {connect}")),
                (Ok(exec) | Err(exec), connect) => {
                    let (Ok(connect) | Err(connect)) = connect;
                    Err(format!("{exec}; TCP: {connect}"))
                }
            },
        };
        Ok(Probed { result, tail })
    }

    /// The exec part of a probe: the check's command run in `container` through the exec
    /// slots. The result is the exit code (or the timeout); for a deployment the output's tail,
    /// redacted by `log`, comes with it separately.
    async fn exec<R: ContainerRuntime>(
        &self,
        probes: &Probes<'_, R>,
        container: &ContainerId,
        mode: Mode<'_>,
    ) -> Result<Probed, HealthError> {
        let Kind::Exec { command, .. } = &self.kind else {
            unreachable!("only an exec check runs in the container");
        };
        let at_cap = match mode {
            Mode::Deploy { deadline, .. } => AtCap::WaitUntil(deadline),
            Mode::Live => AtCap::Refuse,
        };
        let outcome = match probes
            .execs
            .run(probes.runtime, container, command, self.timeout, at_cap)
            .await
        {
            Ok(outcome) => outcome,
            Err(SlotError::Stuck(running)) => {
                return Ok(Err(format!("check stuck: {running} checks still running")).into());
            }
            Err(SlotError::Runtime(
                RuntimeError::NotRunning { .. } | RuntimeError::NotFound { .. },
            )) => return Err(HealthError::Stopping(container.clone())),
            Err(SlotError::Runtime(err)) => return Ok(Err(format!("cannot exec: {err}")).into()),
        };
        let result = match &outcome {
            ExecOutcome::CannotRun { reason, .. } => {
                let argv0 = command.first().map_or("", String::as_str);
                return Err(HealthError::CannotRun(
                    CannotRun { argv0, reason }.to_string(),
                ));
            }
            ExecOutcome::Exited { .. } | ExecOutcome::TimedOut { .. } => match outcome.code() {
                Some(code) if outcome.passed() => Ok(format!("exit {code}")),
                Some(code) => Err(format!("exit {code}")),
                None => Err(format!(
                    "no result within {}",
                    humantime::format_duration(self.timeout)
                )),
            },
        };
        let tail = match mode {
            Mode::Deploy { log, .. } => {
                let output = log.redact(outcome.output().as_ref());
                let tail = Tail(&output);
                (!tail.is_empty()).then(|| tail.to_string())
            }
            Mode::Live => None,
        };
        Ok(Probed { result, tail })
    }

    /// One network probe, bounded by `timeout`: `Ok` with what passed, `Err` with why it failed.
    pub(crate) async fn attempt(&self) -> Result<String, String> {
        let probe = async {
            match &self.kind {
                Kind::Http {
                    addr, path, expect, ..
                } => {
                    let status = Self::http_status(*addr, path).await?;
                    if expect.matches(status) {
                        Ok(format!("HTTP {status}"))
                    } else {
                        Err(format!("HTTP {status}, expected {expect}"))
                    }
                }
                Kind::Tcp { addr, .. } => TcpStream::connect(addr)
                    .await
                    .map(|_| "connected".to_owned())
                    .map_err(|err| format!("cannot connect: {err}")),
                // `probe_once` runs an exec check in the container, never through a prober.
                Kind::Exec { .. } => unreachable!("an exec check runs in the container"),
                Kind::Settle => Ok("running".to_owned()),
            }
        };
        tokio::time::timeout(self.timeout, probe)
            .await
            .unwrap_or_else(|_| {
                Err(format!(
                    "no answer within {}",
                    humantime::format_duration(self.timeout)
                ))
            })
    }

    /// Sends `GET path` over HTTP/1.1 and returns the status code. Redirects are not followed:
    /// a 3xx is the answer.
    async fn http_status(addr: SocketAddr, path: &str) -> Result<u16, String> {
        let mut stream = TcpStream::connect(addr)
            .await
            .map_err(|err| format!("cannot connect: {err}"))?;
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUser-Agent: hoster-health-check\r\n\
             Accept: */*\r\nConnection: close\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|err| format!("cannot send the request: {err}"))?;
        let mut head = Vec::new();
        let mut buf = [0u8; 256];
        // RFC 9112 §2.2: a bare LF ends a line too.
        let end = loop {
            if let Some(end) = head.iter().position(|&b| b == b'\n') {
                break end;
            }
            if head.len() >= MAX_STATUS_LINE {
                return Err("no HTTP status line in the answer".into());
            }
            let n = stream
                .read(&mut buf)
                .await
                .map_err(|err| format!("cannot read the answer: {err}"))?;
            if n == 0 {
                return Err("connection closed before an HTTP status line".into());
            }
            head.extend_from_slice(&buf[..n]);
        };
        let line = head[..end].strip_suffix(b"\r").unwrap_or(&head[..end]);
        let line = String::from_utf8_lossy(line);
        Self::parse_status_line(&line)
            .ok_or_else(|| format!("invalid HTTP status line `{}`", line.escape_debug()))
    }

    /// The status code of `HTTP/1.x NNN [reason]` (RFC 9112 §4): exactly three digits.
    fn parse_status_line(line: &str) -> Option<u16> {
        let rest = line.strip_prefix("HTTP/1.")?;
        let (minor, rest) = rest.split_once(' ')?;
        let code = rest.split(' ').next()?;
        let well_formed = minor.len() == 1
            && minor.bytes().all(|b| b.is_ascii_digit())
            && code.len() == 3
            && code.bytes().all(|b| b.is_ascii_digit());
        well_formed.then(|| code.parse().ok()).flatten()
    }
}

/// How the check reads in events: `GET /healthz on container port 8080 (127.0.0.1:20001)`,
/// `exec: pg_isready -U postgres`. The alternate form (`{check:#}`) adds why it applies:
/// `exec: pg_isready -U postgres, declared in health.exec`.
impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            Kind::Http {
                addr,
                container_port,
                path,
                ..
            } => write!(f, "GET {path} on container port {container_port} ({addr})")?,
            Kind::Tcp {
                addr,
                container_port,
            } => write!(f, "TCP connect to container port {container_port} ({addr})")?,
            Kind::Exec { command, connect } => {
                write!(f, "exec: {}", Command(command))?;
                if let Some((addr, container_port)) = connect {
                    write!(
                        f,
                        " and TCP connect to container port {container_port} ({addr})"
                    )?;
                }
            }
            Kind::Settle => write!(
                f,
                "running for {}",
                humantime::format_duration(self.start_period)
            )?,
        }
        if !f.alternate() {
            return Ok(());
        }
        match (self.origin, &self.kind) {
            (Origin::Declared, Kind::Http { .. }) => f.write_str(", declared in health.http"),
            (Origin::Declared, Kind::Tcp { .. }) => f.write_str(", declared in health.tcp"),
            (Origin::Declared, Kind::Exec { connect: None, .. }) => {
                f.write_str(", declared in health.exec")
            }
            (Origin::Declared, Kind::Exec { .. }) => f.write_str(
                ", declared in health.exec; a routed service's exec check also connects to its \
                 first route's port",
            ),
            (Origin::RoutedDefault, _) => {
                f.write_str(", the default for a routed service without health")
            }
            // A settle check is never declared: `settle()` is the unrouted default.
            (Origin::UnroutedDefault, _) | (Origin::Declared, Kind::Settle) => {
                f.write_str(", the default for a service without routes or health")
            }
        }
    }
}

/// A duration rounded to milliseconds, for event text: `3s 241ms`.
pub(crate) struct Elapsed(pub(crate) Duration);

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = Duration::from_millis(u64::try_from(self.0.as_millis()).unwrap_or(u64::MAX));
        write!(f, "{}", humantime::format_duration(millis))
    }
}

/// Where probe results go.
#[async_trait]
pub trait ProbeLog: Send + Sync {
    async fn record(&self, message: &str);

    /// `text` (an exec's output) with whatever must not be recorded taken out; before it is
    /// cut to its tail, so a secret is never cut in two.
    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }
}

/// Discards the results.
#[async_trait]
impl ProbeLog for () {
    async fn record(&self, _message: &str) {}
}

/// Records probe results as events of a deployment in the state it is in (`HealthChecking`
/// or `Replacing`), with the secret values of its containers redacted from exec output.
pub struct DeploymentLog {
    store: Store,
    deployment: DeploymentId,
    state: DeploymentState,
    /// Secret values to redact, longest first.
    secrets: Vec<SecretValue>,
    /// Held while an event is written, so the writers sharing it store their events in the
    /// order of their timestamps.
    writes: Arc<tokio::sync::Mutex<()>>,
}

impl DeploymentLog {
    /// What [`Self::redact`] puts in place of a secret value.
    pub const REDACTED: &str = "<redacted>";
    /// Shorter secret values are not redacted: they would match everywhere (architecture §5.5).
    pub const MIN_REDACTED_CHARS: usize = 4;

    pub fn new(store: Store, deployment: DeploymentId, state: DeploymentState) -> Self {
        Self {
            store,
            deployment,
            state,
            secrets: Vec::new(),
            writes: Arc::default(),
        }
    }

    /// Writes its events one at a time with every other writer holding `writes`, e.g. the
    /// logs of probes running in parallel and the deploy task's own notes.
    pub fn serialized(mut self, writes: &Arc<tokio::sync::Mutex<()>>) -> Self {
        self.writes = Arc::clone(writes);
        self
    }

    /// Also redacts `secrets` (those of [`Self::MIN_REDACTED_CHARS`] or more) from exec output.
    pub fn redacting<'a>(mut self, secrets: impl IntoIterator<Item = &'a SecretValue>) -> Self {
        self.secrets.extend(
            secrets
                .into_iter()
                .filter(|s| s.expose().chars().count() >= Self::MIN_REDACTED_CHARS)
                .cloned(),
        );
        // A secret containing another is redacted whole.
        self.secrets.sort_by(|a, b| {
            let (a, b) = (a.expose(), b.expose());
            b.len().cmp(&a.len()).then_with(|| a.cmp(b))
        });
        self.secrets.dedup();
        self
    }
}

#[async_trait]
impl ProbeLog for DeploymentLog {
    async fn record(&self, message: &str) {
        let _writing = self.writes.lock().await;
        // The store also writes the event to hoster's log (target `hoster::deploy`).
        if let Err(err) = self.store.note(self.deployment, self.state, message).await {
            tracing::warn!(deployment = %self.deployment, %err, "cannot record a health event");
        }
    }

    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut text = Cow::Borrowed(text);
        for secret in &self.secrets {
            if text.contains(secret.expose()) {
                text = Cow::Owned(text.replace(secret.expose(), Self::REDACTED));
            }
        }
        text
    }
}

#[cfg(test)]
mod tests;
