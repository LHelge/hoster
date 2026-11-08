//! Image garbage collection (ADR-0041 and its correction).
//!
//! Each service keeps its current digest and the digests of its last [`GcSettings::keep`]
//! successful deployments, so a rollback works even when the registry is unreachable; digests of
//! deployments still in flight are kept too. Every other image in the `images` table (what
//! hoster pulled) is removed through the runtime once a day. Images hoster did not pull are not
//! in the table and are never touched. A removal that fails (an image a container still uses)
//! keeps its record and is tried again on the next pass.
//!
//! A pass removes nothing while any deployment, reconcile pass or stack removal is running, and
//! each removal holds the orchestrator's image lock exclusively, which a deployment holds shared
//! while it pulls. A deployment that starts during a removal therefore pulls only after it, and
//! one already past its pull keeps the pass from removing anything until it finishes.
//!
//! Containers of old generations need no pass of their own: a deployment removes them in
//! `CleaningUp`, and the reconciler removes leftovers (ADR-0028). Volumes are never pruned
//! (ADR-0030).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{Orchestrator, Result};
use crate::runtime::{ContainerRuntime, RuntimeError};
use crate::stack::PinnedImage;
use crate::store::{ImageUse, ImageUseKind, RecordedImage};

/// Tunables of image garbage collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcSettings {
    /// How many successful digests each service keeps besides its current one.
    pub keep: usize,
    /// The time between passes.
    pub interval: Duration,
    /// The wait before the first pass, and before another try of a pass that was deferred
    /// because a deployment was running.
    pub retry: Duration,
}

impl Default for GcSettings {
    fn default() -> Self {
        Self {
            keep: 3,
            interval: Duration::from_secs(24 * 60 * 60),
            retry: Duration::from_secs(10 * 60),
        }
    }
}

/// The digests no image garbage collection may remove: the union of every service's retention
/// set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Retention {
    kept: BTreeSet<String>,
}

impl Retention {
    /// The retention sets of every service in `uses`: its current digest, any in-flight
    /// deployment's digest, and the distinct digests of its newest `keep` successful
    /// deployments other than the current one.
    pub(super) fn new(keep: usize, uses: &[ImageUse]) -> Self {
        let mut kept = BTreeSet::new();
        let mut services: BTreeMap<(&str, &str), Service<'_>> = BTreeMap::new();
        for image in uses {
            let service = services
                .entry((image.stack.as_str(), image.service.as_str()))
                .or_default();
            match image.kind {
                ImageUseKind::Current => service.current = Some(&image.digest),
                ImageUseKind::InFlight => {
                    kept.insert(image.digest.clone());
                }
                ImageUseKind::Live(id) => service.live.push((id, &image.digest)),
            }
        }
        for mut service in services.into_values() {
            service.live.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
            let mut earlier: Vec<&str> = Vec::new();
            for (_, digest) in &service.live {
                if earlier.len() == keep {
                    break;
                }
                if Some(*digest) != service.current && !earlier.contains(digest) {
                    earlier.push(digest);
                }
            }
            kept.extend(service.current.map(str::to_owned));
            kept.extend(earlier.into_iter().map(str::to_owned));
        }
        Self { kept }
    }

    /// Whether some service keeps `digest`.
    pub(super) fn keeps(&self, digest: &str) -> bool {
        self.kept.contains(digest)
    }
}

/// One service's digests while its retention set is worked out.
#[derive(Default)]
struct Service<'a> {
    current: Option<&'a str>,
    live: Vec<(crate::store::DeploymentId, &'a str)>,
}

/// What one garbage collection pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// The images removed, as `registry/repository@digest`.
    pub removed: Vec<String>,
    /// Recorded images kept because a service's retention set names them.
    pub retained: usize,
    /// Images whose removal failed, with why; their records stay for the next pass.
    pub failed: Vec<(String, String)>,
    /// The pass stopped early because a deployment started; the rest waits for the next try.
    pub deferred: bool,
}

impl<R: ContainerRuntime> Orchestrator<R> {
    /// One image garbage collection pass: removes every recorded image no service keeps.
    #[tracing::instrument(skip(self))]
    pub async fn collect_garbage(&self) -> Result<GcReport> {
        let mut report = GcReport::default();
        if self.deploying() {
            report.deferred = true;
            return Ok(report);
        }
        let retention = Retention::new(self.settings.gc.keep, &self.store.image_uses().await?);
        for recorded in self.store.recorded_images().await? {
            if retention.keeps(&recorded.digest) {
                report.retained += 1;
                continue;
            }
            // A deployment that started since the retention was read may want this image. One
            // that is pulling holds the lock shared; one that becomes active while this removal
            // runs waits for it in `Pulling` and then pulls the image again, so a removal never
            // takes an image from under a deployment that already found it present.
            let Ok(_removing) = self.images.try_write() else {
                report.deferred = true;
                break;
            };
            if self.deploying() {
                report.deferred = true;
                break;
            }
            self.prune(&recorded, &mut report).await?;
        }
        tracing::info!(
            removed = report.removed.len(),
            retained = report.retained,
            failed = report.failed.len(),
            deferred = report.deferred,
            "image garbage collection pass"
        );
        Ok(report)
    }

    /// Removes one recorded image and forgets it, unless the runtime refuses.
    async fn prune(&self, recorded: &RecordedImage, report: &mut GcReport) -> Result<()> {
        let reference = format!("{}@{}", recorded.repository, recorded.digest);
        let image: PinnedImage = match reference.parse() {
            Ok(image) => image,
            Err(err) => {
                tracing::warn!(image = %reference, %err, "recorded image is not a valid reference");
                report.failed.push((reference, err));
                return Ok(());
            }
        };
        match self.runtime.remove_image(&image).await {
            // Already gone (removed by hand): only the record is left to forget.
            Ok(()) | Err(RuntimeError::NotFound { .. }) => {
                self.store
                    .forget_image(&recorded.repository, &recorded.digest)
                    .await?;
                report.removed.push(reference);
            }
            Err(err) => {
                tracing::warn!(
                    image = %reference, %err,
                    "cannot remove image; trying again at the next pass"
                );
                report.failed.push((reference, err.to_string()));
            }
        }
        Ok(())
    }

    /// Whether a deployment, reconcile pass or removal is working on some stack.
    fn deploying(&self) -> bool {
        self.lock()
            .slots
            .values()
            .any(|slot| slot.active.is_some() || slot.reconciling || slot.removing)
    }

    /// Runs [`Self::collect_garbage`] once a day (after [`GcSettings::retry`] the first time,
    /// and again that soon after a deferred or failed pass) until `cancel` fires.
    pub async fn run_garbage_collector(self: Arc<Self>, cancel: CancellationToken) {
        let GcSettings {
            interval, retry, ..
        } = self.settings.gc;
        let mut wait = retry;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(wait) => {}
            }
            wait = match self.collect_garbage().await {
                Ok(report) if report.deferred => retry,
                Ok(_) => interval,
                Err(err) => {
                    tracing::warn!(%err, "image garbage collection failed; trying again later");
                    retry
                }
            };
        }
        tracing::debug!("image garbage collector stopped");
    }
}
