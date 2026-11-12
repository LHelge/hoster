//! Statistics for the web UI (ADR-0045, ADR-0062). There is no metrics endpoint: the only
//! reader is the authenticated admin app, through the [`Stats`] handle.
//!
//! Traffic comes in two parts:
//!
//! - **Live counters** ([`Traffic`]): per-route request counts by status class, upstream errors
//!   and a latency histogram, updated lock-free by the proxy's `logging` hook. The traffic
//!   recorder ([`TrafficRecorder`]) drains what they counted into the store once a minute, and
//!   on graceful shutdown. The pages read only one live figure from them: the requests in flight
//!   now ([`InFlight`]).
//! - **Traffic history** ([`TrafficHistory`]): what the recorder flushed, 1-minute buckets for
//!   24 h and 1-hour buckets for 60 days, read back per [`Range`] and scope as bars, totals, the
//!   previous period, the current rate and the peak minute. It survives restarts, and lags by up
//!   to a minute, since the minute in progress is never read.
//!
//! Besides traffic:
//!
//! - **Deployments**: outcome counts and durations per stack, read from SQLite on demand.
//! - **Certificates**: the `not_after` of every certificate the proxy serves.
//! - **Resource usage** ([`ResourceUsage`], ADR-0066): container and host CPU, memory and disk,
//!   sampled every 10 s by the [`ResourceSampler`] and kept in memory for an hour.
//!
//! Restarts and crash loops (ADR-0028) are not kept here: pages read them from the stacks'
//! status. Everything but the live counters comes from a [`StatsSource`]; [`HostStats`] is the
//! real one.

mod history;
mod latency;
mod recorder;
mod resources;
mod traffic;

use std::fmt;
use std::ops::Range as Span;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::acme::CertMap;
use crate::store::{
    DeploymentOutcome, DeploymentState, Resolution, Store, Timestamp, TrafficBucket,
};

pub use self::history::{Range, ScopeHistory, TrafficHistory, TrafficTotals};
pub use self::latency::Histogram;
pub use self::recorder::TrafficRecorder;
#[allow(
    unused_imports,
    reason = "named by the sidebar host meters and the stack overview (tasks hzc, rap)"
)]
pub use self::resources::{
    ContainerReading, ContainerSample, ContainerUsage, History, HostSample, Usage,
};
pub use self::resources::{ResourceSampler, ResourceUsage};
pub use self::traffic::{
    InFlight, RequestOutcome, StatusClass, StatusCounts, TrackedRequest, Traffic, TrafficDelta,
};

/// Where the statistics not kept in memory come from.
#[async_trait]
pub trait StatsSource: Send + Sync + 'static {
    /// Deployment outcomes per stack and terminal state, over the last
    /// [`Stats::DEPLOY_HISTORY`].
    async fn deployments(&self) -> anyhow::Result<Vec<DeploymentOutcome>>;

    /// The certificates served now.
    fn certificates(&self) -> Vec<CertExpiry>;

    /// The flushed traffic history buckets of `resolution` that start within `span`, of every
    /// series (ADR-0062).
    async fn traffic(
        &self,
        resolution: Resolution,
        span: Span<Timestamp>,
    ) -> anyhow::Result<Vec<TrafficBucket<Histogram>>>;
}

/// The statistics the admin app shows. A cheap handle: clones share everything.
#[derive(Clone)]
pub struct Stats {
    traffic: Traffic,
    source: Arc<dyn StatsSource>,
    resources: ResourceUsage,
}

impl fmt::Debug for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stats").finish_non_exhaustive()
    }
}

impl Stats {
    /// How far back the deployment statistics look.
    pub const DEPLOY_HISTORY: Duration = Duration::from_secs(30 * 24 * 60 * 60);

    /// Statistics over `traffic` (the proxy's counters) and `source`, with no resource usage
    /// (every figure unknown) until [`Self::with_resources`].
    pub fn new(traffic: Traffic, source: Arc<dyn StatsSource>) -> Self {
        Self {
            traffic,
            source,
            resources: ResourceUsage::default(),
        }
    }

    /// These statistics with the resource sampler's cache (ADR-0066).
    pub fn with_resources(self, resources: ResourceUsage) -> Self {
        Self { resources, ..self }
    }

    /// The resource sampler's cache: host and container CPU, memory and disk (ADR-0066).
    #[allow(
        dead_code,
        reason = "read by the sidebar host meters and the stack overview (tasks hzc, rap)"
    )]
    pub fn resources(&self) -> &ResourceUsage {
        &self.resources
    }

    /// The certificates served now, soonest expiry first.
    pub fn certificates(&self) -> Vec<CertExpiry> {
        let mut certificates = self.source.certificates();
        certificates.sort_by_key(|cert| cert.not_after);
        certificates
    }

    /// The requests in flight now: the one traffic figure read live, not from the history.
    pub fn in_flight(&self) -> InFlight {
        self.traffic.in_flight()
    }

    /// How the deployments of the last [`Self::DEPLOY_HISTORY`] went, per stack, sorted by name.
    pub async fn deployments(&self) -> anyhow::Result<Vec<DeployStats>> {
        Ok(DeployStats::group(self.source.deployments().await?))
    }

    /// The traffic history over `range` as of now, for every scope at once: one read of the
    /// range's table, plus one of the rate window's minutes for ranges drawn from hour buckets.
    pub async fn history(&self, range: Range) -> anyhow::Result<TrafficHistory> {
        self.history_at(range, Timestamp::now()).await
    }

    /// The traffic history over `range` as of `now`; see [`Self::history`].
    pub async fn history_at(&self, range: Range, now: Timestamp) -> anyhow::Result<TrafficHistory> {
        let (span, recent) = TrafficHistory::spans(range, now);
        let rows = self.source.traffic(range.resolution(), span).await?;
        let recent = match recent {
            Some(span) => Some(self.source.traffic(Resolution::Minute, span).await?),
            None => None,
        };
        Ok(TrafficHistory::new(range, now, &rows, recent.as_deref()))
    }
}

/// How the finished deployments of one stack went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployStats {
    pub stack: String,
    /// One entry per terminal state reached, in the store's order.
    pub outcomes: Vec<DeploymentOutcome>,
}

impl DeployStats {
    /// Groups the store's per-(stack, state) rows, sorted by stack, into one entry per stack.
    fn group(outcomes: Vec<DeploymentOutcome>) -> Vec<DeployStats> {
        let mut stacks: Vec<DeployStats> = Vec::new();
        for outcome in outcomes {
            match stacks.last_mut() {
                Some(last) if last.stack == outcome.stack => last.outcomes.push(outcome),
                _ => stacks.push(DeployStats {
                    stack: outcome.stack.clone(),
                    outcomes: vec![outcome],
                }),
            }
        }
        stacks
    }

    /// Deployments that went live, with or without warnings.
    pub fn succeeded(&self) -> u32 {
        self.count(|state| {
            matches!(
                state,
                DeploymentState::Live | DeploymentState::LiveWithWarnings
            )
        })
    }

    /// Deployments that failed or were rejected.
    pub fn failed(&self) -> u32 {
        self.count(|state| matches!(state, DeploymentState::Failed | DeploymentState::Rejected))
    }

    /// Deployments with nothing to do, or replaced while queued.
    pub fn skipped(&self) -> u32 {
        self.count(|state| matches!(state, DeploymentState::Noop | DeploymentState::Superseded))
    }

    /// The mean duration of the deployments that went live.
    pub fn mean_live_duration(&self) -> Option<Duration> {
        let live = self.live();
        let count: u32 = live.clone().map(|o| o.count).sum();
        let total: Duration = live.map(|o| o.mean * o.count).sum();
        (count > 0).then(|| total / count)
    }

    /// The longest deployment that went live.
    pub fn max_live_duration(&self) -> Option<Duration> {
        self.live().map(|o| o.max).max()
    }

    fn live(&self) -> impl Iterator<Item = &DeploymentOutcome> + Clone {
        self.outcomes.iter().filter(|o| {
            matches!(
                o.state,
                DeploymentState::Live | DeploymentState::LiveWithWarnings
            )
        })
    }

    fn count(&self, matches: impl Fn(DeploymentState) -> bool) -> u32 {
        self.outcomes
            .iter()
            .filter(|o| matches(o.state))
            .map(|o| o.count)
            .sum()
    }
}

/// When one served certificate expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertExpiry {
    pub host: String,
    pub not_after: SystemTime,
}

impl CertExpiry {
    /// How long the certificate is still valid at `now`; zero once it has expired.
    pub fn remaining(&self, now: SystemTime) -> Duration {
        self.not_after.duration_since(now).unwrap_or_default()
    }
}

/// The statistics of the running hoster: deployments from the store and certificates from the
/// proxy's [`CertMap`].
pub struct HostStats {
    pub store: Store,
    pub certs: CertMap,
}

#[async_trait]
impl StatsSource for HostStats {
    async fn deployments(&self) -> anyhow::Result<Vec<DeploymentOutcome>> {
        let since = Timestamp::now() - Stats::DEPLOY_HISTORY;
        Ok(self.store.deployment_outcomes(since).await?)
    }

    fn certificates(&self) -> Vec<CertExpiry> {
        self.certs
            .hosts()
            .into_iter()
            .filter_map(|host| {
                let validity = self.certs.validity(&host)?;
                Some(CertExpiry {
                    host,
                    not_after: validity.not_after,
                })
            })
            .collect()
    }

    async fn traffic(
        &self,
        resolution: Resolution,
        span: Span<Timestamp>,
    ) -> anyhow::Result<Vec<TrafficBucket<Histogram>>> {
        Ok(self.store.traffic(resolution, span, None).await?)
    }
}

/// A duration as the UI shows it: `0.8 ms`, `12 ms`, `1.4 s`, `3 min 5 s`, `5 min`, `2 d 4 h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed(pub Duration);

impl Elapsed {
    /// An optional duration for a table cell: `-` when there is none.
    pub fn cell(duration: Option<Duration>) -> String {
        duration.map_or_else(|| "-".to_owned(), |d| Elapsed(d).to_string())
    }
}

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0;
        let secs = d.as_secs();
        if d < Duration::from_millis(10) {
            write!(f, "{:.1} ms", d.as_secs_f64() * 1000.0)
        } else if d < Duration::from_secs(1) {
            write!(f, "{} ms", d.as_millis())
        } else if d < Duration::from_secs(60) {
            write!(f, "{:.1} s", d.as_secs_f64())
        } else if secs < 60 * 60 {
            Self::two_units(f, (secs / 60, "min"), (secs % 60, "s"))
        } else if secs < 24 * 60 * 60 {
            Self::two_units(f, (secs / 3600, "h"), (secs % 3600 / 60, "min"))
        } else {
            Self::two_units(f, (secs / 86_400, "d"), (secs % 86_400 / 3600, "h"))
        }
    }
}

impl Elapsed {
    /// `3 min 5 s`, or `5 min` when the smaller unit is zero.
    fn two_units(
        f: &mut fmt::Formatter<'_>,
        (big, big_unit): (u64, &str),
        (small, small_unit): (u64, &str),
    ) -> fmt::Result {
        write!(f, "{big} {big_unit}")?;
        if small > 0 {
            write!(f, " {small} {small_unit}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests;
