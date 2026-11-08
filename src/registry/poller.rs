//! The edge-triggered digest poller (ADR-0005, ADR-0043 as amended by ADR-0051 #1).
//!
//! [`Poller::run`] wakes every `poller.interval` (5 minutes by default) plus a random jitter of
//! up to a tenth of it, and resolves the tag of every service whose stack file sets
//! `track = "<tag>"` to the digest the registry serves for it. The **last observed digest** of
//! each (repository, tag) is persisted in the state store, and a deploy (`source = poller`) is
//! issued only on an edge:
//!
//! - **First sighting** of a (repository, tag) by a tracking service: the observed digest is
//!   deployed if the service **has never run** (it has no service pointer); a service that runs
//!   keeps what it runs and the digest is only recorded (ADR-0051 #1). Each observation also
//!   records which services it was made for, so a service that starts tracking a tag the poller
//!   already knows (a second stack, or a worker added on the same image) has its own first
//!   sighting and is bootstrapped too.
//! - **Afterwards**: a service is deployed only when the observed digest **changed** since the
//!   last poll *and* differs from the digest it runs. An unchanged tag never deploys, so a
//!   rollback made in the UI (ADR-0041) holds until the tag moves again, a webhook that already
//!   deployed the new digest makes the poll a no-op, and a first deploy that failed is not
//!   retried until the next release.
//!
//! Services of one stack that are due are deployed together in one request. An observation is
//! recorded only once its deploy was accepted, so a refused one is retried on the next poll.
//! Observations of tags, and of services, that no longer track are dropped, so tracking again
//! (a stack removed and applied again) counts as a first sighting.
//!
//! A tag whose lookup fails backs off on its own, without holding up the others: the next
//! attempt waits `interval × 2^(failures − 1)`, at most [`Poller::MAX_BACKOFF`], and at least as
//! long as a `429`'s `Retry-After` asks. The backoff lives in memory; a restart polls again.
//!
//! The orchestrator, the registry and the observation store are reached through
//! [`PollTarget`], [`TagResolver`] and [`ObservedStore`], so the unit tests drive the poller
//! with fakes and paused time.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::RegistryError;
use crate::config::PollerConfig;
use crate::orchestrator::{Orchestrator, TagResolver};
use crate::runtime::ContainerRuntime;
use crate::stack::{Digest, ImageRef};
use crate::store::{DeploymentSource, DigestSet, Store};

/// A service whose stack file sets `track`, as the poller sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedService {
    pub stack: String,
    pub service: String,
    /// The service's image with the tracked tag: the reference the poller resolves.
    pub image: ImageRef,
    /// The digest the service runs; `None` when it has never run.
    pub running: Option<Digest>,
}

impl TrackedService {
    /// How an [`Observation`] names the service: `stack/service`.
    fn id(&self) -> String {
        format!("{}/{}", self.stack, self.service)
    }
}

/// What the poller needs of the orchestrator. `Arc<Orchestrator>` in production.
#[async_trait]
pub trait PollTarget: Send + Sync {
    /// Every tracked service of every stack.
    async fn tracked(&self) -> anyhow::Result<Vec<TrackedService>>;
    /// Deploys `images` (service → digest) to `stack`, with `source = poller`.
    async fn deploy(&self, stack: &str, images: DigestSet) -> anyhow::Result<()>;
}

#[async_trait]
impl<R: ContainerRuntime + 'static> PollTarget for Arc<Orchestrator<R>> {
    async fn tracked(&self) -> anyhow::Result<Vec<TrackedService>> {
        Ok(Orchestrator::tracked(self).await?)
    }

    async fn deploy(&self, stack: &str, images: DigestSet) -> anyhow::Result<()> {
        let outcome = Orchestrator::deploy(self, stack, images, DeploymentSource::Poller).await?;
        tracing::info!(
            stack,
            deployment = %outcome.deployment,
            state = %outcome.state,
            "the poller requested a deployment"
        );
        Ok(())
    }
}

/// The last observed digest of one tracked tag, and the services it was observed for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub digest: String,
    /// The tracking services (`stack/service`) that have had their first sighting.
    pub services: BTreeSet<String>,
}

/// The last observation of every tracked tag, by `registry/repository:tag`.
pub type Observed = BTreeMap<String, Observation>;

/// Where the observations survive restarts. [`Store`] in production.
#[async_trait]
pub trait ObservedStore: Send + Sync {
    async fn load(&self) -> anyhow::Result<Observed>;
    async fn save(&self, observed: &Observed) -> anyhow::Result<()>;
}

/// The key-value item the observations are stored under.
const OBSERVED_KEY: &str = "poller.observed";

#[async_trait]
impl ObservedStore for Store {
    async fn load(&self) -> anyhow::Result<Observed> {
        match self.kv_get(OBSERVED_KEY).await? {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => Ok(Observed::new()),
        }
    }

    async fn save(&self, observed: &Observed) -> anyhow::Result<()> {
        if observed.is_empty() {
            self.kv_remove(OBSERVED_KEY).await?;
        } else {
            self.kv_set(OBSERVED_KEY, &serde_json::to_string(observed)?)
                .await?;
        }
        Ok(())
    }
}

/// The failed lookups of one tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Backoff {
    failures: u32,
    retry_at: Instant,
}

impl Backoff {
    /// The backoff after another failure `err` at `now`, following `previous`.
    fn after(
        previous: Option<&Backoff>,
        err: &RegistryError,
        interval: Duration,
        now: Instant,
    ) -> Self {
        let failures = previous.map_or(0, |b| b.failures).saturating_add(1);
        let doublings = failures.saturating_sub(1).min(31);
        let delay = interval
            .saturating_mul(1u32 << doublings)
            .min(Poller::MAX_BACKOFF.max(interval))
            .max(err.retry_after().unwrap_or_default());
        Self {
            failures,
            retry_at: now + delay,
        }
    }
}

/// The digest poller. Build it with [`Poller::new`] and [`run`](Poller::run) it.
pub struct Poller {
    target: Box<dyn PollTarget>,
    resolver: Arc<dyn TagResolver>,
    store: Arc<dyn ObservedStore>,
    interval: Duration,
    /// The longest random delay added to each interval.
    jitter: Duration,
    /// Tags whose lookup is failing, by observation key.
    backoff: HashMap<String, Backoff>,
}

impl Poller {
    /// The longest a failing tag waits between lookups (unless the interval is longer).
    pub const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

    /// A poller deploying through `target`, resolving tags with `resolver` and persisting what
    /// it observed in `store`, at the interval of `config`.
    pub fn new(
        target: Box<dyn PollTarget>,
        resolver: Arc<dyn TagResolver>,
        store: Arc<dyn ObservedStore>,
        config: &PollerConfig,
    ) -> Self {
        Self {
            target,
            resolver,
            store,
            interval: config.interval,
            jitter: config.interval / 10,
            backoff: HashMap::new(),
        }
    }

    /// Replaces the longest random delay added to each interval (a tenth of it by default).
    #[cfg(test)]
    pub fn with_jitter(mut self, jitter: Duration) -> Self {
        self.jitter = jitter;
        self
    }

    /// Polls until `token` is cancelled: the first time after a random jitter, then every
    /// interval plus jitter. A failed poll is logged and the next one runs as usual.
    pub async fn run(mut self, token: CancellationToken) {
        tracing::info!(
            interval = %humantime::format_duration(self.interval),
            "digest poller started"
        );
        let mut delay = self.random_jitter();
        loop {
            tokio::select! {
                () = token.cancelled() => break,
                () = tokio::time::sleep(delay) => {}
            }
            tokio::select! {
                () = token.cancelled() => break,
                result = self.poll() => {
                    if let Err(err) = result {
                        tracing::warn!(error = format!("{err:#}"), "digest poll failed");
                    }
                }
            }
            delay = self.interval + self.random_jitter();
        }
        tracing::debug!("digest poller stopped");
    }

    /// A uniformly random delay in `0..=jitter`.
    fn random_jitter(&self) -> Duration {
        let max = u64::try_from(self.jitter.as_millis()).unwrap_or(u64::MAX);
        if max == 0 {
            return Duration::ZERO;
        }
        let random = getrandom::u64().unwrap_or_default();
        Duration::from_millis(random % (max + 1))
    }

    /// One pass over every tracked tag.
    pub async fn poll(&mut self) -> anyhow::Result<()> {
        let mut tags: BTreeMap<String, (ImageRef, Vec<TrackedService>)> = BTreeMap::new();
        for service in self.target.tracked().await? {
            tags.entry(service.image.to_string())
                .or_insert_with(|| (service.image.clone(), Vec::new()))
                .1
                .push(service);
        }
        let mut observed = self.store.load().await?;
        let before = observed.clone();
        // What no service tracks any more is forgotten: tracking it again is a first sighting.
        observed.retain(|key, observation| {
            let Some((_, services)) = tags.get(key) else {
                return false;
            };
            observation
                .services
                .retain(|id| services.iter().any(|s| s.id() == *id));
            true
        });
        self.backoff.retain(|key, _| tags.contains_key(key));

        // What each stack is due, and the observations to record once that deploy is accepted.
        let mut due: BTreeMap<String, DigestSet> = BTreeMap::new();
        let mut waiting: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut seen: BTreeMap<String, Observation> = BTreeMap::new();
        for (key, (image, services)) in &tags {
            let Some(digest) = self.observe(key, image).await else {
                continue;
            };
            for service in services {
                // The digest last observed for this service; `None` on its first sighting.
                let previous = observed
                    .get(key)
                    .filter(|o| o.services.contains(&service.id()))
                    .map(|o| o.digest.as_str());
                if Self::is_due(previous, &digest, service.running.as_ref()) {
                    tracing::info!(
                        stack = service.stack,
                        service = service.service,
                        image = %image,
                        %digest,
                        first_sighting = previous.is_none(),
                        "tracked tag moved; deploying"
                    );
                    due.entry(service.stack.clone())
                        .or_default()
                        .insert(service.service.clone(), digest.to_string());
                    waiting
                        .entry(service.stack.clone())
                        .or_default()
                        .insert(key.clone());
                }
            }
            let observation = Observation {
                digest: digest.to_string(),
                services: services.iter().map(TrackedService::id).collect(),
            };
            seen.insert(key.clone(), observation);
        }

        let mut refused = BTreeSet::new();
        for (stack, images) in due {
            if let Err(err) = self.target.deploy(&stack, images).await {
                tracing::warn!(
                    stack,
                    error = format!("{err:#}"),
                    "the poller's deploy was refused; retrying on the next poll"
                );
                refused.extend(waiting.remove(&stack).unwrap_or_default());
            }
        }
        for (key, observation) in seen {
            if !refused.contains(&key) {
                observed.insert(key, observation);
            }
        }
        if observed != before {
            self.store.save(&observed).await?;
        }
        Ok(())
    }

    /// Whether a service running `running` is deployed when its tag, last observed for it at
    /// `previous` (`None` on its first sighting), now resolves to `digest`.
    fn is_due(previous: Option<&str>, digest: &Digest, running: Option<&Digest>) -> bool {
        match previous {
            // First sighting: bootstrap a service that never ran, leave a running one alone.
            None => running.is_none(),
            Some(previous) => previous != digest.as_ref() && running != Some(digest),
        }
    }

    /// Resolves `image`'s tag, unless it is backing off. A failure starts or extends its backoff.
    async fn observe(&mut self, key: &str, image: &ImageRef) -> Option<Digest> {
        let now = Instant::now();
        if self.backoff.get(key).is_some_and(|b| b.retry_at > now) {
            return None;
        }
        match self.resolver.resolve(image).await {
            Ok(pinned) => {
                self.backoff.remove(key);
                Some(pinned.digest().clone())
            }
            Err(err) => {
                let backoff = Backoff::after(self.backoff.get(key), &err, self.interval, now);
                tracing::warn!(
                    image = %image,
                    error = %err,
                    failures = backoff.failures,
                    retry_in = %humantime::format_duration(backoff.retry_at - now),
                    "cannot resolve a tracked tag"
                );
                self.backoff.insert(key.to_owned(), backoff);
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "poller_tests.rs"]
mod tests;
