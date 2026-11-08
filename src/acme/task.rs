//! The ACME background task (ADR-0015, ADR-0036, ADR-0051, docs/architecture.md §7.3).
//!
//! [`AcmeTask::run`] owns the set of hosts that need a certificate: the admin host, always, plus
//! every host the orchestrator sends in an [`AcmeCommand::Ensure`] and has not withdrawn with
//! [`AcmeCommand::Forget`]. A host is issued a certificate when it has none (or only an expired
//! one), and renewed once less than a third of its lifetime remains or the CA's ARI window has
//! opened. Every wanted host is looked at on an hourly tick; a forgotten host keeps its
//! certificate until it expires but is never renewed (ADR-0036).
//!
//! Failures back off per host, exponentially from 5 minutes to 24 hours (ADR-0035 #5). The
//! backoff is persisted in the state store, so a restart does not reset it.
//!
//! Before each attempt the host's A/AAAA records are compared with hoster's public addresses
//! (the admin host's records, re-resolved hourly, or `acme.public_addresses`). The check is
//! advisory (ADR-0051 #2): a mismatch is reported as [`AcmeEvent::DnsMismatch`] and issuance is
//! attempted anyway.
//!
//! Every outcome goes to the orchestrator as an [`AcmeEvent`] (`acme_events`, §3.3 #2), which
//! records it and relays it on `ui_events`.
//!
//! The CA, DNS, the backoff store and the clock are reached through the [`Acme`], [`Resolve`],
//! [`BackoffStore`] and [`Clock`] traits so that the unit tests can drive the task with fakes
//! and paused time. [`AcmeService`] is the production wiring: it registers (or restores) the
//! account, retrying until the CA answers, and then runs the task with the real [`Issuer`].

use std::collections::{BTreeMap, BTreeSet};
use std::future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use super::account::AccountStore;
use super::certs::{CertMap, Validity};
use super::challenge::ChallengeMap;
use super::dns::{DnsCheck, DnsMismatch, DnsVerdict, Resolve};
use super::issue::{IssueError, Issuer, RenewalWindow};
use crate::config::{AcmeConfig, Config};
use crate::store::Store;

/// What the orchestrator asks of the ACME task (architecture §3.3 #1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcmeCommand {
    /// These hosts are routed: issue a certificate for each one that has no valid one, and keep
    /// renewing them.
    Ensure { hosts: Vec<String> },
    /// The host is no longer routed: keep its certificate until it expires, but stop renewing it.
    Forget { host: String },
    /// The operator asked to try `host` again now (the certificates page). Like any attempt it
    /// is skipped while the host's backoff runs and when its certificate is not due.
    Retry { host: String },
}

/// What the ACME task reports to the orchestrator (architecture §3.3 #2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcmeEvent {
    /// A certificate was issued and is served from now on.
    Issued { host: String, not_after: SystemTime },
    /// An issuance failed; the next attempt is not before `retry_at`.
    Failed {
        host: String,
        error: String,
        retry_at: SystemTime,
    },
    /// The host's DNS records do not point at hoster; issuance is attempted anyway (ADR-0051).
    DnsMismatch(DnsMismatch),
}

impl AcmeEvent {
    /// The host the event is about.
    pub fn host(&self) -> &str {
        match self {
            AcmeEvent::Issued { host, .. } | AcmeEvent::Failed { host, .. } => host,
            AcmeEvent::DnsMismatch(mismatch) => &mismatch.host,
        }
    }
}

/// The CA and the certificates it issued, as far as the task needs them. [`Issuer`] in
/// production; the tests use a fake.
#[async_trait]
pub trait Acme: Send + Sync {
    /// When `host`'s served certificate is valid, if it has one.
    fn validity(&self, host: &str) -> Option<Validity>;
    /// The CA's suggested renewal window for `host`'s certificate, if the CA supports ARI.
    async fn renewal_window(&self, host: &str) -> Option<RenewalWindow>;
    /// Issues and installs a certificate for `host`.
    async fn issue(&self, host: &str) -> Result<Validity, IssueError>;
}

#[async_trait]
impl Acme for Issuer {
    fn validity(&self, host: &str) -> Option<Validity> {
        Issuer::validity(self, host)
    }

    async fn renewal_window(&self, host: &str) -> Option<RenewalWindow> {
        Issuer::renewal_window(self, host).await
    }

    async fn issue(&self, host: &str) -> Result<Validity, IssueError> {
        Issuer::issue(self, host).await
    }
}

/// Wall-clock time, which certificate lifetimes and persisted backoffs are measured in.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

/// [`SystemTime::now`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// One host's failed attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backoff {
    /// Consecutive failed attempts.
    pub failures: u32,
    /// No attempt before this time.
    #[serde(with = "humantime_serde")]
    pub retry_at: SystemTime,
}

impl Backoff {
    /// The delay after the first failure (ADR-0035 #5).
    pub const INITIAL: Duration = Duration::from_secs(5 * 60);
    /// The longest delay.
    pub const MAX: Duration = Duration::from_secs(24 * 60 * 60);

    /// The backoff after another failure at `now`, following `previous`.
    pub fn after(previous: Option<&Backoff>, now: SystemTime) -> Self {
        let failures = previous.map_or(0, |b| b.failures).saturating_add(1);
        Self {
            failures,
            retry_at: now + Self::delay(failures),
        }
    }

    /// How long is left of the backoff at `now`; `None` once an attempt is allowed again.
    pub fn remaining(&self, now: SystemTime) -> Option<Duration> {
        self.retry_at
            .duration_since(now)
            .ok()
            .filter(|left| !left.is_zero())
    }

    /// How long to wait after `failures` consecutive failures: 5 min, doubling, at most 24 h.
    pub fn delay(failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(31);
        Self::INITIAL
            .saturating_mul(1u32 << doublings)
            .min(Self::MAX)
    }
}

/// The persisted backoff of every host that is failing.
pub type Backoffs = BTreeMap<String, Backoff>;

/// Where the backoffs survive restarts. [`Store`] in production.
#[async_trait]
pub trait BackoffStore: Send + Sync {
    async fn load(&self) -> anyhow::Result<Backoffs>;
    async fn save(&self, backoffs: &Backoffs) -> anyhow::Result<()>;
}

/// The key-value item the backoffs are stored under.
const BACKOFF_KEY: &str = "acme.backoff";

#[async_trait]
impl BackoffStore for Store {
    async fn load(&self) -> anyhow::Result<Backoffs> {
        match self.kv_get(BACKOFF_KEY).await? {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(Backoffs::new()),
        }
    }

    async fn save(&self, backoffs: &Backoffs) -> anyhow::Result<()> {
        if backoffs.is_empty() {
            self.kv_remove(BACKOFF_KEY).await?;
        } else {
            self.kv_set(BACKOFF_KEY, &serde_json::to_string(backoffs)?)
                .await?;
        }
        Ok(())
    }
}

/// The ACME background task. Build it with [`AcmeTask::new`] and [`run`](AcmeTask::run) it.
pub struct AcmeTask {
    acme: Arc<dyn Acme>,
    store: Arc<dyn BackoffStore>,
    clock: Arc<dyn Clock>,
    admin_host: String,
    dns: DnsCheck,
    events: mpsc::Sender<AcmeEvent>,
    tick: Duration,
}

impl AcmeTask {
    /// How often every wanted host is checked for renewal and the public addresses are
    /// re-resolved.
    pub const TICK: Duration = Duration::from_secs(60 * 60);

    /// A task issuing through `acme` for `admin_host` and the ensured hosts, persisting backoffs
    /// in `store`, checking DNS with `dns` and reporting on `events`.
    pub fn new(
        acme: Arc<dyn Acme>,
        store: Arc<dyn BackoffStore>,
        admin_host: impl Into<String>,
        dns: impl Into<DnsCheck>,
        events: mpsc::Sender<AcmeEvent>,
    ) -> Self {
        Self {
            acme,
            store,
            clock: Arc::new(SystemClock),
            admin_host: admin_host.into().to_ascii_lowercase(),
            dns: dns.into(),
            events,
            tick: Self::TICK,
        }
    }

    /// Resolves the DNS check's names through `resolver` instead of the system resolver.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "production uses the system resolver")
    )]
    pub fn with_resolver(mut self, resolver: Arc<dyn Resolve>) -> Self {
        self.dns = self.dns.with_resolver(resolver);
        self
    }

    /// Reads wall-clock time from `clock` instead of the system clock.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "production uses the system clock")
    )]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Runs until `shutdown` is cancelled: ensures the admin host's certificate, then serves
    /// `commands`, the renewal tick and the retries of failed hosts. An issuance in flight is
    /// abandoned on shutdown.
    pub async fn run(
        self,
        mut commands: mpsc::UnboundedReceiver<AcmeCommand>,
        shutdown: CancellationToken,
    ) {
        let tick = self.tick;
        let mut worker = Worker::start(self).await;
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + tick, tick);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut commands_open = true;

        worker.check_all(&shutdown).await;
        while !shutdown.is_cancelled() {
            let retry = worker.next_retry();
            let wait = async {
                match retry {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                command = commands.recv(), if commands_open => match command {
                    Some(command) => worker.command(command, &shutdown).await,
                    // The orchestrator is gone; renewals go on until shutdown.
                    None => commands_open = false,
                },
                _ = ticks.tick() => {
                    worker.refresh_public().await;
                    worker.check_all(&shutdown).await;
                }
                () = wait => worker.retry_due(&shutdown).await,
            }
        }
        tracing::debug!("ACME task stopped");
    }
}

/// The running task's state.
struct Worker {
    task: AcmeTask,
    /// The hosts that need a certificate; always includes the admin host.
    wanted: BTreeSet<String>,
    backoffs: Backoffs,
}

impl Worker {
    async fn start(task: AcmeTask) -> Self {
        let backoffs = match task.store.load().await {
            Ok(backoffs) => backoffs,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "cannot load the ACME backoffs"
                );
                Backoffs::new()
            }
        };
        let wanted = BTreeSet::from([task.admin_host.clone()]);
        let worker = Self {
            task,
            wanted,
            backoffs,
        };
        worker.refresh_public().await;
        worker
    }

    async fn command(&mut self, command: AcmeCommand, shutdown: &CancellationToken) {
        match command {
            AcmeCommand::Ensure { hosts } => {
                let hosts: BTreeSet<String> =
                    hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
                self.wanted.extend(hosts.iter().cloned());
                for host in &hosts {
                    if shutdown.is_cancelled() {
                        return;
                    }
                    self.check(host, shutdown).await;
                }
            }
            AcmeCommand::Forget { host } => {
                let host = host.to_ascii_lowercase();
                if host == self.task.admin_host {
                    return;
                }
                if self.wanted.remove(&host) {
                    tracing::info!(
                        host,
                        "host no longer routed; its certificate is not renewed"
                    );
                }
                if self.backoffs.remove(&host).is_some() {
                    self.persist().await;
                }
            }
            AcmeCommand::Retry { host } => {
                let host = host.to_ascii_lowercase();
                if self.wanted.contains(&host) {
                    self.check(&host, shutdown).await;
                }
            }
        }
    }

    /// Re-resolves hoster's public addresses, keeping the previous ones on a failure.
    async fn refresh_public(&self) {
        self.task.dns.refresh().await;
    }

    /// Checks every wanted host.
    async fn check_all(&mut self, shutdown: &CancellationToken) {
        for host in self.wanted.clone() {
            if shutdown.is_cancelled() {
                return;
            }
            self.check(&host, shutdown).await;
        }
    }

    /// Checks the wanted hosts whose backoff has run out.
    async fn retry_due(&mut self, shutdown: &CancellationToken) {
        let now = self.task.clock.now();
        let due: Vec<String> = self
            .backoffs
            .iter()
            .filter(|(host, b)| b.retry_at <= now && self.wanted.contains(*host))
            .map(|(host, _)| host.clone())
            .collect();
        for host in due {
            if shutdown.is_cancelled() {
                return;
            }
            self.check(&host, shutdown).await;
        }
    }

    /// How long until the earliest retry of a wanted host that is backing off.
    fn next_retry(&self) -> Option<Duration> {
        let now = self.task.clock.now();
        self.backoffs
            .iter()
            .filter(|(host, _)| self.wanted.contains(*host))
            .map(|(_, b)| b.retry_at.duration_since(now).unwrap_or_default())
            .min()
    }

    /// Issues a certificate for `host` if it needs one and is not backing off.
    async fn check(&mut self, host: &str, shutdown: &CancellationToken) {
        let now = self.task.clock.now();
        if self.backoffs.get(host).is_some_and(|b| b.retry_at > now) {
            return;
        }
        if !self.due(host, now).await {
            // A certificate arrived some other way; a stale backoff must not keep waking us.
            if self.backoffs.remove(host).is_some() {
                self.persist().await;
            }
            return;
        }
        if let DnsVerdict::Mismatch(mismatch) = self.task.dns.check(host).await {
            tracing::warn!(
                host,
                observed = ?mismatch.observed,
                expected = ?mismatch.expected,
                "DNS does not point at hoster; requesting the certificate anyway"
            );
            self.emit(AcmeEvent::DnsMismatch(mismatch)).await;
        }
        let issued = tokio::select! {
            biased;
            () = shutdown.cancelled() => return,
            issued = self.task.acme.issue(host) => issued,
        };
        let event = match issued {
            Ok(validity) => {
                if self.backoffs.remove(host).is_some() {
                    self.persist().await;
                }
                AcmeEvent::Issued {
                    host: host.to_owned(),
                    not_after: validity.not_after,
                }
            }
            Err(error) => {
                let now = self.task.clock.now();
                let backoff = Backoff::after(self.backoffs.get(host), now);
                self.backoffs.insert(host.to_owned(), backoff);
                self.persist().await;
                tracing::warn!(
                    host,
                    failures = backoff.failures,
                    retry_in = %humantime::format_duration(Backoff::delay(backoff.failures)),
                    "certificate issuance failed; backing off"
                );
                AcmeEvent::Failed {
                    host: host.to_owned(),
                    error: format!("{:#}", anyhow::Error::from(error)),
                    retry_at: backoff.retry_at,
                }
            }
        };
        self.emit(event).await;
    }

    /// Whether `host` needs a (new) certificate at `now`.
    async fn due(&self, host: &str, now: SystemTime) -> bool {
        let Some(validity) = self.task.acme.validity(host) else {
            return true;
        };
        if validity.is_expired_at(now) || validity.renew_at() <= now {
            return true;
        }
        self.task
            .acme
            .renewal_window(host)
            .await
            .is_some_and(|window| window.start <= now)
    }

    async fn persist(&self) {
        if let Err(error) = self.task.store.save(&self.backoffs).await {
            tracing::warn!(
                error = format!("{error:#}"),
                "cannot persist the ACME backoffs"
            );
        }
    }

    async fn emit(&self, event: AcmeEvent) {
        // The orchestrator stops listening only when hoster shuts down.
        let _ = self.task.events.send(event).await;
    }
}

/// The production ACME task: the account from `acme/account.json` (registered on first use) and
/// the real [`Issuer`], answering challenges on `:80` and installing certificates for `:443`.
pub struct AcmeService {
    pub config: AcmeConfig,
    pub state_dir: PathBuf,
    pub admin_host: String,
    pub dns: DnsCheck,
    pub challenges: ChallengeMap,
    pub certs: CertMap,
    pub store: Store,
}

impl AcmeService {
    /// How long to wait before asking the CA for the account again.
    const ACCOUNT_RETRY: Duration = Duration::from_secs(5 * 60);

    /// The service for hoster's configuration, sharing `dns` (and the public addresses it
    /// resolves) with the web UI.
    pub fn new(
        config: &Config,
        challenges: ChallengeMap,
        certs: CertMap,
        store: Store,
        dns: DnsCheck,
    ) -> Self {
        Self {
            config: config.acme.clone(),
            state_dir: config.state_dir.clone(),
            admin_host: config.admin_host.to_string(),
            dns,
            challenges,
            certs,
            store,
        }
    }

    /// Loads or registers the account, retrying until the CA answers, then runs the
    /// [`AcmeTask`] until `shutdown`. Commands sent meanwhile wait in the channel.
    pub async fn run(
        self,
        commands: mpsc::UnboundedReceiver<AcmeCommand>,
        events: mpsc::Sender<AcmeEvent>,
        shutdown: CancellationToken,
    ) {
        let accounts = AccountStore::new(&self.state_dir);
        let account = loop {
            let attempt = tokio::select! {
                () = shutdown.cancelled() => return,
                attempt = accounts.load_or_create(&self.config) => attempt,
            };
            match attempt {
                Ok(account) => break account,
                Err(error) => {
                    tracing::error!(
                        error = format!("{:#}", anyhow::Error::from(error)),
                        retry_in = %humantime::format_duration(Self::ACCOUNT_RETRY),
                        "cannot set up the ACME account; no certificates are issued meanwhile"
                    );
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(Self::ACCOUNT_RETRY) => {}
                    }
                }
            }
        };
        let issuer = Issuer::new(account, self.challenges, self.certs);
        AcmeTask::new(
            Arc::new(issuer),
            Arc::new(self.store),
            self.admin_host,
            self.dns,
            events,
        )
        .run(commands, shutdown)
        .await;
    }
}

#[cfg(test)]
#[path = "task_tests.rs"]
mod tests;
