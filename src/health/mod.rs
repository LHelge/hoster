//! Health checks of new containers (ADR-0038, as amended by ADR-0051 #3).
//!
//! hoster probes a container **from the host**, on the `127.0.0.1` port allocated for one of its
//! route ports, so distroless images need no curl. The public surface is small:
//!
//! - [`Check::for_service`] resolves a service's declared `health` table (or the defaults) and
//!   its allocated [`PortMap`] into a [`Check`]: an `http` probe, a `tcp` probe, or, for a
//!   service without routes, "running and not restarted for 5 s".
//! - [`Check::run`] carries it out against one container until it passes, its `start_period`
//!   runs out, or the container exits (checked with `inspect` before every probe).
//! - Results go to a [`ProbeLog`]; [`DeploymentLog`] records them as deployment events.
//! - After a deploy, [`Check::live`] probes a live container once, through a [`Prober`], and
//!   [`Liveness`] turns the results into a [`HealthStatus`] (see `liveness`).
//!
//! hoster turns podman's restart policy off (ADR-0028), so "running at every look" is "running
//! and not restarted".

#![allow(
    dead_code,
    reason = "the deploy engine switches to real probes in a later task (shg)"
)]

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

use crate::runtime::{ContainerId, ContainerRuntime, ContainerState, RuntimeError};
use crate::stack::{DEFAULT_HEALTH_PATH, ExpectStatus, Probe, Service};
use crate::store::{DeploymentId, DeploymentState, PortMap, Store};

mod liveness;

pub use liveness::{
    HealthChange, HealthStats, HealthStatus, Liveness, MonitorSettings, NetworkProber, Prober,
};

/// Time between two probes when `interval` is unset.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(1);
/// How long one probe may take when `timeout` is unset.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
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
    /// Running and not restarted for `start_period`.
    Settle,
}

/// Why a container did not become healthy.
#[derive(Debug, Error)]
pub enum HealthError {
    #[error("health probe port {0} has no allocated loopback port")]
    NoLoopbackPort(u16),
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
    /// (container port → loopback port). A declared `health` table wins; otherwise a routed
    /// service gets `GET /healthz` on its first route's port, expecting 2xx–3xx (ADR-0051), and
    /// a service without routes gets "running and not restarted for 5 s".
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
                Some(route) => Self::probe(http(
                    DEFAULT_HEALTH_PATH,
                    route.port,
                    ExpectStatus::default(),
                )?),
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
        };
        let mut check = Self::probe(kind);
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
        }
    }

    fn probe(kind: Kind) -> Self {
        Self {
            kind,
            interval: DEFAULT_INTERVAL,
            timeout: DEFAULT_TIMEOUT,
            healthy_after: DEFAULT_HEALTHY_AFTER,
            start_period: DEFAULT_START_PERIOD,
        }
    }

    /// Overrides the time between probes (or inspections).
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Overrides how long the container gets (or, for the settle check, must stay up).
    pub fn with_start_period(mut self, start_period: Duration) -> Self {
        self.start_period = start_period;
        self
    }

    /// Consecutive passes required.
    pub fn healthy_after(&self) -> u32 {
        self.healthy_after
    }

    /// One liveness probe of a container known to be running: through `prober` for an HTTP or
    /// TCP check; the settle check has nothing to probe, running is passing.
    pub async fn live(&self, prober: &dyn Prober) -> Result<String, String> {
        match self.kind {
            Kind::Settle => Ok("running".to_owned()),
            Kind::Http { .. } | Kind::Tcp { .. } => prober.probe(self).await,
        }
    }

    /// Runs the check against `container` until it passes, `start_period` runs out or the
    /// container stops running. Probe results, and the verdict, go to `log`; a result is
    /// logged when it differs from the previous one, so a steady failure is one event.
    pub async fn run<R: ContainerRuntime>(
        &self,
        runtime: &R,
        container: &ContainerId,
        log: &dyn ProbeLog,
    ) -> Result<(), HealthError> {
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
                let result = self.attempt().await;
                streak = if result.is_ok() { streak + 1 } else { 0 };
                if streak >= self.healthy_after {
                    log.record(&format!(
                        "health check {self} passed {streak} time(s) in a row after {}",
                        Elapsed(started.elapsed())
                    ))
                    .await;
                    return Ok(());
                }
                if last.as_ref() != Some(&result) {
                    let (Ok(text) | Err(text)) = &result;
                    log.record(&format!("health probe {self}: {text}")).await;
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

    /// One probe, bounded by `timeout`: `Ok` with what passed, `Err` with why it failed.
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

/// How the check reads in events: `GET /healthz on container port 8080 (127.0.0.1:20001)`.
impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            Kind::Http {
                addr,
                container_port,
                path,
                ..
            } => write!(f, "GET {path} on container port {container_port} ({addr})"),
            Kind::Tcp {
                addr,
                container_port,
            } => write!(f, "TCP connect to container port {container_port} ({addr})"),
            Kind::Settle => write!(
                f,
                "running for {}",
                humantime::format_duration(self.start_period)
            ),
        }
    }
}

/// A duration rounded to milliseconds, for event text.
struct Elapsed(Duration);

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
}

/// Discards the results.
#[async_trait]
impl ProbeLog for () {
    async fn record(&self, _message: &str) {}
}

/// Records probe results as events of a deployment in the state it is in (`HealthChecking`
/// or `Replacing`).
pub struct DeploymentLog {
    store: Store,
    deployment: DeploymentId,
    state: DeploymentState,
}

impl DeploymentLog {
    pub fn new(store: Store, deployment: DeploymentId, state: DeploymentState) -> Self {
        Self {
            store,
            deployment,
            state,
        }
    }
}

#[async_trait]
impl ProbeLog for DeploymentLog {
    async fn record(&self, message: &str) {
        // The store also writes the event to hoster's log (target `hoster::deploy`).
        if let Err(err) = self.store.note(self.deployment, self.state, message).await {
            tracing::warn!(deployment = %self.deployment, %err, "cannot record a health event");
        }
    }
}

#[cfg(test)]
mod tests;
