//! The update checker (ADR-0061 §1–§3, §6): finds updates of third-party images and stores
//! them. It only detects; applying an update is the apply preview's or auto-update's job.
//!
//! [`UpdateChecker::run`] runs a pass every [`CheckInterval`] (6 hours by default, set in
//! Settings → Updates) with ±10 % jitter. The time of the last pass is persisted, so a restart
//! continues the schedule instead of checking at once; the first pass ever runs
//! [`UpdateChecker::FIRST_PASS_DELAY`] after start. An [`UpdateCheckHandle`] asks for a pass
//! now ("Check now") or for the schedule to be read again (the interval changed).
//!
//! A pass checks every service the targets name ([`ImageCheck`]): a `HEAD` of its tag (a
//! different digest than the revision pins is a [`UpdateKind::Digest`] update) and, when the
//! tag reads as a [`TagVersion`], the repository's tag list for the newest patch, minor and
//! major in the same line. Each tag is resolved and each repository listed at most once per
//! pass. A rate limit, a network error or a 5xx stops the pass from asking that registry again;
//! a `Retry-After` beyond the pass keeps the next passes away until it has passed. The
//! services it could not check keep their earlier candidates, with the error recorded.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{RegistryClient, RegistryError};
use crate::orchestrator::{Orchestrator, UpdateEvents};
use crate::runtime::ContainerRuntime;
use crate::stack::{Bump, Digest, ImageRef, PinnedImage, TagVersion};
use crate::store::{Candidate, ImageCheck, NewEvent, Store, Timestamp, UpdateKind};

/// What the checker needs of the orchestrator. `Arc<Orchestrator>` in production.
#[async_trait]
pub trait UpdateTargets: Send + Sync {
    /// Every checkable service of every active revision.
    async fn checkable(&self) -> anyhow::Result<Vec<ImageCheck>>;
    /// Records `event` in the event log.
    async fn record(&self, event: NewEvent);
    /// Tells the open pages that `stack`'s updates were checked.
    fn announce(&self, stack: &str);
}

#[async_trait]
impl<R: ContainerRuntime + 'static> UpdateTargets for Arc<Orchestrator<R>> {
    async fn checkable(&self) -> anyhow::Result<Vec<ImageCheck>> {
        Ok(Orchestrator::checkable(self).await?)
    }

    async fn record(&self, event: NewEvent) {
        self.event_log().record(event).await;
    }

    fn announce(&self, stack: &str) {
        self.announce_updates(stack);
    }
}

/// What the checker asks of registries. [`RegistryClient`] in production.
#[async_trait]
pub trait UpdateRegistry: Send + Sync {
    /// `image`'s tag, pinned to the digest the registry serves for it now.
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError>;
    /// Every tag of `image`'s repository.
    async fn tags(&self, image: &ImageRef) -> Result<Vec<String>, RegistryError>;
}

#[async_trait]
impl UpdateRegistry for RegistryClient {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError> {
        RegistryClient::resolve(self, image).await
    }

    async fn tags(&self, image: &ImageRef) -> Result<Vec<String>, RegistryError> {
        RegistryClient::tags(self, image).await
    }
}

/// A request to the running checker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    /// Run a pass now.
    CheckNow,
    /// Read the settings again and recompute when the next pass is due.
    Reschedule,
}

/// Asks the running [`UpdateChecker`] for a pass now, or to read its schedule again. Cheap to
/// clone. Requests made while one is pending are merged into it.
#[derive(Debug, Clone)]
pub struct UpdateCheckHandle(mpsc::Sender<Wake>);

#[allow(dead_code, reason = "called from Settings → Updates (Bears j6w)")]
impl UpdateCheckHandle {
    /// Runs a pass as soon as the checker is idle ("Check now").
    pub fn check_now(&self) {
        let _ = self.0.try_send(Wake::CheckNow);
    }

    /// Recomputes when the next pass is due, after the interval was changed.
    pub fn reschedule(&self) {
        let _ = self.0.try_send(Wake::Reschedule);
    }
}

/// The update checker. Build it with [`UpdateChecker::new`] and [`run`](UpdateChecker::run) it.
pub struct UpdateChecker {
    targets: Box<dyn UpdateTargets>,
    registry: Arc<dyn UpdateRegistry>,
    store: Store,
    wake_tx: mpsc::Sender<Wake>,
    wake_rx: mpsc::Receiver<Wake>,
    /// Registries that failed as a whole (a rate limit, a network error, a 5xx): until when a
    /// pass leaves them alone, and why.
    backoff: HashMap<String, (Instant, String)>,
}

/// The key-value item holding the wall-clock time of the last pass, in Unix milliseconds.
const LAST_PASS_KEY: &str = "updates.last_pass";

/// The largest part of the interval added to or taken from it, at random, per pass.
const JITTER: f64 = 0.1;

impl UpdateChecker {
    /// How long after start the first pass ever runs, so it does not add to startup's load.
    pub const FIRST_PASS_DELAY: Duration = Duration::from_secs(2 * 60);

    /// A checker of what `targets` names, asking `registry` and recording in `store`.
    pub fn new(
        targets: Box<dyn UpdateTargets>,
        registry: Arc<dyn UpdateRegistry>,
        store: Store,
    ) -> Self {
        let (wake_tx, wake_rx) = mpsc::channel(1);
        Self {
            targets,
            registry,
            store,
            wake_tx,
            wake_rx,
            backoff: HashMap::new(),
        }
    }

    /// A handle that asks this checker for a pass or a new schedule while it runs.
    #[allow(dead_code, reason = "handed to Settings → Updates (Bears j6w)")]
    pub fn handle(&self) -> UpdateCheckHandle {
        UpdateCheckHandle(self.wake_tx.clone())
    }

    /// Checks on schedule until `token` is cancelled. A failed pass is logged, and the next one
    /// runs as scheduled.
    pub async fn run(mut self, token: CancellationToken) {
        tracing::info!("update checker started");
        let mut jitter = Self::random_jitter();
        loop {
            let delay = self.next_delay(jitter).await;
            let wake = tokio::select! {
                () = token.cancelled() => break,
                () = tokio::time::sleep(delay) => Wake::CheckNow,
                wake = self.wake_rx.recv() => wake.unwrap_or(Wake::Reschedule),
            };
            if wake == Wake::Reschedule {
                continue;
            }
            tokio::select! {
                () = token.cancelled() => break,
                result = self.check() => {
                    if let Err(err) = result {
                        tracing::warn!(error = format!("{err:#}"), "update check failed");
                    }
                }
            }
            jitter = Self::random_jitter();
        }
        tracing::debug!("update checker stopped");
    }

    /// How long until the next pass is due, with `jitter` (a fraction of the interval).
    async fn next_delay(&self, jitter: f64) -> Duration {
        let interval = match self.store.update_settings().await {
            Ok(settings) => settings.interval.into(),
            Err(err) => {
                tracing::warn!(
                    error = format!("{:#}", anyhow::Error::from(err)),
                    "cannot read the update settings; using the default interval"
                );
                Duration::from(crate::store::CheckInterval::DEFAULT)
            }
        };
        let last = match self.store.kv_get(LAST_PASS_KEY).await {
            Ok(last) => last.and_then(|millis| millis.parse::<i64>().ok()),
            Err(err) => {
                tracing::warn!(
                    error = format!("{:#}", anyhow::Error::from(err)),
                    "cannot read the time of the last update check"
                );
                None
            }
        };
        Self::delay(last, Timestamp::now().as_millis(), interval, jitter)
    }

    /// The delay until the next pass, when the last one ran at `last` (Unix milliseconds;
    /// `None` when none has) and it is `now`.
    fn delay(last: Option<i64>, now: i64, interval: Duration, jitter: f64) -> Duration {
        let Some(last) = last else {
            return Self::FIRST_PASS_DELAY;
        };
        let interval = interval.mul_f64(1.0 + jitter.clamp(-JITTER, JITTER));
        let elapsed = u64::try_from(now.saturating_sub(last)).unwrap_or(0);
        interval.saturating_sub(Duration::from_millis(elapsed))
    }

    /// A uniformly random fraction in `-JITTER..=JITTER`.
    fn random_jitter() -> f64 {
        let random = getrandom::u64().unwrap_or_default() % 2001;
        (random as f64 / 1000.0 - 1.0) * JITTER
    }

    /// One pass over every checkable service.
    pub async fn check(&mut self) -> anyhow::Result<()> {
        let services = self.targets.checkable().await?;
        let keep: BTreeSet<(String, String)> = services
            .iter()
            .map(|s| (s.stack.clone(), s.service.clone()))
            .collect();
        self.store.retain_image_checks(&keep).await?;
        let now = Instant::now();
        self.backoff.retain(|_, (until, _)| *until > now);

        let mut pass = Pass::default();
        let mut checked = BTreeSet::new();
        for check in &services {
            match self.candidates(check, &mut pass).await {
                Ok(candidates) => {
                    let new = self.store.record_image_check(check, &candidates).await?;
                    for candidate in &new {
                        tracing::info!(
                            stack = check.stack,
                            service = check.service,
                            image = %check.image,
                            kind = %candidate.kind,
                            tag = candidate.tag,
                            "update available"
                        );
                        self.targets
                            .record(UpdateEvents::available(check, candidate))
                            .await;
                    }
                }
                Err(error) => {
                    self.store.record_image_check_error(check, &error).await?;
                }
            }
            checked.insert(check.stack.as_str());
        }
        for stack in checked {
            self.targets.announce(stack);
        }
        self.store
            .kv_set(LAST_PASS_KEY, &Timestamp::now().as_millis().to_string())
            .await?;
        Ok(())
    }

    /// The updates of `check`'s service, or why they could not be found.
    async fn candidates(
        &mut self,
        check: &ImageCheck,
        pass: &mut Pass,
    ) -> Result<Vec<Candidate>, String> {
        let tag = check.image.tag().unwrap_or_default();
        let mut candidates = Vec::new();
        let served = self.served_digest(check, pass).await?;
        if served != check.digest {
            candidates.push(Candidate {
                kind: UpdateKind::Digest,
                tag: tag.to_owned(),
                digest: Some(served),
            });
        }
        if let Ok(current) = tag.parse::<TagVersion>() {
            let tags = self.tag_list(check, pass).await?;
            for bump in [Bump::Patch, Bump::Minor, Bump::Major] {
                let newest = current
                    .newest_in_line(tags.iter().map(String::as_str), bump)
                    .filter(|newest| current.bump_kind(newest) == Some(bump));
                if let Some(newest) = newest {
                    candidates.push(Candidate {
                        kind: bump.into(),
                        tag: newest.to_string(),
                        digest: None,
                    });
                }
            }
        }
        Ok(candidates)
    }

    /// The digest `check`'s tag points at now, asked once per pass.
    async fn served_digest(
        &mut self,
        check: &ImageCheck,
        pass: &mut Pass,
    ) -> Result<Digest, String> {
        let key = format!(
            "{}/{}:{}",
            check.image.registry(),
            check.image.repository(),
            check.image.tag().unwrap_or_default()
        );
        if let Some(served) = pass.digests.get(&key) {
            return served.clone();
        }
        self.backing_off(check.image.registry())?;
        let served = match self.registry.resolve(&check.image).await {
            Ok(pinned) => Ok(pinned.digest().clone()),
            Err(err) => Err(self.failed(check.image.registry(), &err)),
        };
        pass.digests.insert(key, served.clone());
        served
    }

    /// The tags of `check`'s repository, listed once per pass.
    async fn tag_list(
        &mut self,
        check: &ImageCheck,
        pass: &mut Pass,
    ) -> Result<Arc<Vec<String>>, String> {
        let key = format!("{}/{}", check.image.registry(), check.image.repository());
        if let Some(tags) = pass.tags.get(&key) {
            return tags.clone();
        }
        self.backing_off(check.image.registry())?;
        let tags = match self.registry.tags(&check.image).await {
            Ok(tags) => Ok(Arc::new(tags)),
            Err(err) => Err(self.failed(check.image.registry(), &err)),
        };
        pass.tags.insert(key, tags.clone());
        tags
    }

    /// Refuses to ask `registry` while it backs off, with the error that started it.
    fn backing_off(&self, registry: &str) -> Result<(), String> {
        match self.backoff.get(registry) {
            Some((_, error)) => Err(format!("not checked: {error}")),
            None => Ok(()),
        }
    }

    /// Records `err` from `registry`, backing off from it when the registry as a whole failed,
    /// and returns the error text.
    fn failed(&mut self, registry: &str, err: &RegistryError) -> String {
        let error = err.to_string();
        if err.is_registry_wide() {
            let until = Instant::now() + err.retry_after().unwrap_or_default();
            tracing::warn!(registry, error, "backing off from a registry");
            self.backoff
                .insert(registry.to_owned(), (until, error.clone()));
        } else {
            tracing::debug!(registry, error, "cannot check an image for updates");
        }
        error
    }
}

/// What one pass learned, so it asks each registry once per tag and repository.
#[derive(Default)]
struct Pass {
    digests: HashMap<String, Result<Digest, String>>,
    tags: HashMap<String, Result<Arc<Vec<String>>, String>>,
}

#[cfg(test)]
#[path = "updates_tests.rs"]
mod tests;
