//! Continuous health monitoring of live services (ADR-0038, "after a deploy").
//!
//! [`Orchestrator::run_health_monitor`] probes the current generation of every service with its
//! resolved [`Check`] (declared `health` table, `exec` included, or the defaults), every 10–30 s
//! ([`MonitorSettings`]). An exec check runs in the generation's container and reports its exit
//! code only; its output goes to deployment events alone (ADR-0056). Each status change —
//! `Healthy`, `Unhealthy` or `CrashLooping`, the last one taken from the reconciler (ADR-0028) —
//! is logged, kept as in-process statistics (ADR-0045, shown by [`Orchestrator::status`]) and
//! sent on `ui_events` as [`UiEvent::Health`]. A move between healthy and unhealthy is also
//! written to the event log (ADR-0055) with the probe detail; a flapping service writes one
//! event per real transition, never one per probe. Nothing is restarted here.
//!
//! Stacks with an active deployment or a removal are left alone (their last status is kept):
//! the deployment owns their containers and runs its own checks. One that a deployment or
//! removal takes while its probes run has that pass's results dropped.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures_util::future::join_all;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::event_log::ServiceEvents;
use super::{Orchestrator, Result, UiEvent};
use crate::health::{Check, HealthChange, HealthStats, HealthStatus, Liveness, Probes};
use crate::runtime::{ContainerId, ContainerRuntime, ContainerState, ServiceGeneration};
use crate::stack::{Name, Stack};

/// What the monitor remembers of one live generation.
#[derive(Debug)]
pub(super) struct Watched {
    liveness: Liveness,
    /// The next probe is due then.
    next: Instant,
}

/// Every watched generation.
pub(super) type Watchlist = BTreeMap<ServiceGeneration, Watched>;

/// One probe to carry out in a pass.
struct Job {
    key: ServiceGeneration,
    check: Result<Check, String>,
    /// The generation's container and its state, if it has one; an exec check runs in it.
    container: Option<(ContainerId, ContainerState)>,
}

impl Job {
    async fn run<R: ContainerRuntime>(
        &self,
        probes: &Probes<'_, R>,
    ) -> std::result::Result<String, String> {
        let check = self.check.as_ref().map_err(Clone::clone)?;
        match &self.container {
            None => Err("the current generation has no container".to_owned()),
            Some((_, state)) if !state.is_running() => Err(format!("the container is {state}")),
            Some((id, _)) => check.live(probes, id).await,
        }
    }
}

impl<R: ContainerRuntime> Orchestrator<R> {
    /// The health monitor loop: a pass whenever a probe falls due, and at least every
    /// [`MonitorSettings::min_interval`](crate::health::MonitorSettings) to notice new
    /// generations. Ends when `cancel` fires.
    pub async fn run_health_monitor(self: Arc<Self>, cancel: CancellationToken) {
        let refresh = self.settings.monitor.min_interval;
        loop {
            let wake = match self.monitor_at(Instant::now()).await {
                Ok(_) => {
                    let now = Instant::now();
                    self.next_probe()
                        .map_or(now + refresh, |at| at.min(now + refresh))
                }
                Err(err) => {
                    // The probes that were due are still due; wait a refresh before trying
                    // again rather than retrying at once (podman or the store is down).
                    tracing::warn!(%err, "health monitor pass failed; retrying at the next one");
                    Instant::now() + refresh
                }
            };
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep_until(wake) => {}
            }
        }
        tracing::debug!("health monitor stopped");
    }

    /// One monitor pass as if it ran at `now`: probes every live generation whose probe is due
    /// and returns the status changes it caused (also logged and sent on `ui_events`).
    pub(super) async fn monitor_at(self: &Arc<Self>, now: Instant) -> Result<Vec<UiEvent>> {
        let containers = self.runtime.list_managed().await?;
        let mut seen = BTreeSet::new();
        let mut held = BTreeSet::new();
        let mut jobs = Vec::new();
        let mut events = Vec::new();
        // The changes the event log keeps (ADR-0055): only real moves between healthy and
        // unhealthy, see [`ServiceEvents::health_changed`].
        let mut recorded = Vec::new();
        for stack in self.store.stacks().await? {
            if self.is_busy(&stack.name) || self.is_removing(&stack.name) {
                held.insert(stack.name);
                continue;
            }
            let Some(revision) = self.store.latest_revision(&stack.name).await? else {
                continue;
            };
            let Ok(definition) = revision.toml.parse::<Stack>() else {
                continue;
            };
            let Ok(stack_name) = stack.name.parse::<Name>() else {
                continue;
            };
            for pointer in self.store.services(&stack.name).await? {
                let Some((service_name, service)) = definition
                    .services
                    .iter()
                    .find(|(name, _)| name.as_ref() == pointer.service)
                else {
                    continue;
                };
                let key = ServiceGeneration {
                    stack: stack_name.clone(),
                    service: service_name.clone(),
                    generation: pointer.generation,
                };
                seen.insert(key.clone());
                let check = Check::for_service(service, &pointer.ports);
                let interval = self.settings.monitor.interval_for(service);
                let restarts =
                    self.restart_stats(&stack.name, &pointer.service, pointer.generation);
                let due = {
                    let mut watchlist = self.watchlist();
                    let watched = watchlist.entry(key.clone()).or_insert_with(|| Watched {
                        liveness: check
                            .as_ref()
                            .map_or_else(|_| Liveness::new(1), Liveness::from),
                        next: now,
                    });
                    if restarts.crash_looping {
                        watched.next = now + interval;
                        let detail = format!(
                            "the reconciler restarted it {} times; crash looping",
                            restarts.restarts
                        );
                        if let Some(change) = watched.liveness.crash_looping(now, detail) {
                            recorded.extend(ServiceEvents::from(&key).health_changed(&change));
                            events.push(UiEvent::health(&key, change));
                        }
                        false
                    } else if now >= watched.next {
                        watched.next = now + interval;
                        true
                    } else {
                        false
                    }
                };
                if due {
                    let container = containers
                        .iter()
                        .find(|c| ServiceGeneration::try_from(*c).as_ref() == Ok(&key))
                        .map(|c| (c.id.clone(), c.state));
                    jobs.push(Job {
                        key,
                        check: check.map_err(|err| err.to_string()),
                        container,
                    });
                }
            }
        }

        let probes = Probes::new(&*self.runtime, &*self.prober, &self.execs);
        let results = join_all(jobs.iter().map(|job| job.run(&probes))).await;
        // A deployment or removal that started while the probes ran owns the containers now:
        // its stack's results say nothing about the generation it is replacing.
        let taken: BTreeSet<&str> = jobs
            .iter()
            .map(|job| job.key.stack.as_ref())
            .filter(|stack| self.is_busy(stack) || self.is_removing(stack))
            .collect();
        {
            let mut watchlist = self.watchlist();
            for (job, result) in jobs.iter().zip(results) {
                if taken.contains(job.key.stack.as_ref()) {
                    held.insert(job.key.stack.to_string());
                    continue;
                }
                if let Some(watched) = watchlist.get_mut(&job.key)
                    && let Some(change) = watched.liveness.observe(now, result)
                {
                    recorded.extend(ServiceEvents::from(&job.key).health_changed(&change));
                    events.push(UiEvent::health(&job.key, change));
                }
            }
            watchlist.retain(|key, _| seen.contains(key) || held.contains(key.stack.as_ref()));
            // A held stack is looked at again a refresh from now, not at a probe time that
            // has passed (which would wake the loop at once, again and again).
            let refresh = now + self.settings.monitor.min_interval;
            for (_, watched) in watchlist
                .iter_mut()
                .filter(|(key, _)| held.contains(key.stack.as_ref()))
            {
                watched.next = watched.next.max(refresh);
            }
        }
        for event in &events {
            event.log();
            self.emit(event.clone());
        }
        for event in recorded {
            self.events.record(event).await;
        }
        Ok(events)
    }

    /// The health statistics of `stack`'s `service` at `generation`.
    pub(super) fn health_stats(&self, stack: &str, service: &str, generation: u32) -> HealthStats {
        self.watchlist()
            .iter()
            .find(|(key, _)| {
                key.stack.as_ref() == stack
                    && key.service.as_ref() == service
                    && key.generation == generation
            })
            .map(|(_, w)| w.liveness.stats().clone())
            .unwrap_or_default()
    }

    /// When the earliest probe falls due.
    pub(super) fn next_probe(&self) -> Option<Instant> {
        self.watchlist().values().map(|w| w.next).min()
    }
}

impl UiEvent {
    fn health(key: &ServiceGeneration, change: HealthChange) -> Self {
        Self::Health {
            stack: key.stack.to_string(),
            service: key.service.to_string(),
            generation: key.generation,
            from: change.from,
            to: change.to,
            detail: change.detail,
        }
    }

    /// Writes a health change to hoster's log: `warn` when it gets worse, `info` otherwise.
    fn log(&self) {
        let Self::Health {
            stack,
            service,
            generation,
            from,
            to,
            detail,
        } = self
        else {
            return;
        };
        match to {
            HealthStatus::Unhealthy | HealthStatus::CrashLooping => tracing::warn!(
                stack, service, generation, %from, %to, detail, "service health changed"
            ),
            HealthStatus::Healthy | HealthStatus::Unknown => tracing::info!(
                stack, service, generation, %from, %to, detail, "service health changed"
            ),
        }
    }
}
