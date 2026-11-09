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
//!
//! **Start period (ADR-0056 Decision 3).** A (re)start (hoster's own, or a new container ID or
//! `StartedAt` the monitor sees) resets the status to `Unknown`. For min(`start_period`, 5 min)
//! from then, or from when the monitor first saw the container running, it probes at the
//! deploy interval until the first pass, and failures keep the status `Unknown` and write no
//! event. The first pass wakes the reconciler, which may start the services waiting for it:
//! whether a generation passed since its (re)start is what the reconciler's gate reads
//! ([`Orchestrator::readiness`]). The settle check passes once the container has run for its
//! period since then. Dependencies and checks come from the last committed deployment's
//! revision, the one that runs.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_util::future::join_all;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::event_log::ServiceEvents;
use super::{Orchestrator, Result, UiEvent};
use crate::health::{
    Check, HealthChange, HealthError, HealthStats, HealthStatus, Liveness, Probes,
};
use crate::runtime::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerState, ServiceGeneration,
};
use crate::stack::{Name, Service};
use crate::store::PortMap;

/// What the monitor remembers of one live generation.
#[derive(Debug)]
pub(super) struct Watched {
    liveness: Liveness,
    /// The next probe is due then.
    next: Instant,
    /// The container last seen running, with podman's start time when it reports one.
    container: Option<(ContainerId, Option<SystemTime>)>,
    /// When hoster (re)started the container or the monitor first saw it running.
    since: Option<Instant>,
    /// The check passed since `since`.
    passed: bool,
    /// No pass since `since` yet: failures within the grace write no event.
    grace_open: bool,
    /// Seen not running since it last ran: running again is a restart.
    down: bool,
    /// The status was unhealthy when a (re)start reset it: the first pass is "healthy again".
    recovering: bool,
    /// min(`start_period`, 5 min) of the check.
    grace: Duration,
    /// The deploy interval: how often it is probed within the grace.
    interval: Duration,
    /// The settle check passes once the container has run for `grace` since `since`.
    settle: bool,
}

/// Whether a dependency passed its check since its (re)start (ADR-0056 Decision 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Readiness {
    Passed,
    /// Not yet; the reconciler's hold on its dependents ends at `until`.
    Pending {
        until: Instant,
    },
}

/// What one probe result did to a [`Watched`] generation.
#[derive(Debug, Default)]
struct Observed {
    change: Option<HealthChange>,
    /// The first pass since the (re)start.
    first_pass: bool,
    /// That pass ends an unhealthy spell that a restart interrupted.
    recovered: bool,
}

impl Watched {
    /// A generation not looked at yet, probed with `check`.
    fn new(check: &std::result::Result<Check, HealthError>, now: Instant) -> Self {
        let (liveness, grace, interval, settle) = match check {
            Ok(check) => (
                Liveness::from(check),
                check.grace(),
                check.interval(),
                check.is_settle(),
            ),
            Err(_) => (Liveness::new(1), Duration::ZERO, Duration::ZERO, false),
        };
        Self {
            liveness,
            next: now,
            container: None,
            since: None,
            passed: false,
            grace_open: false,
            down: false,
            recovering: false,
            grace,
            interval,
            settle,
        }
    }

    /// What a pass sees of the container at `now`; a (re)start resets the status.
    fn see(&mut self, container: Option<&ContainerInfo>, now: Instant) -> Option<HealthChange> {
        let Some(container) = container.filter(|c| c.state.is_running()) else {
            self.down |= self.since.is_some();
            self.passed = false;
            return None;
        };
        let restarted = self.down
            || self.container.as_ref().is_some_and(|(id, at)| {
                *id != container.id
                    || matches!((at, container.started_at), (Some(a), Some(b)) if *a != b)
            });
        let change = if self.since.is_none() {
            self.begin(now);
            None
        } else if restarted {
            self.reset(now)
        } else {
            None
        };
        self.container = Some((container.id.clone(), container.started_at));
        change
    }

    /// Hoster (re)started the container at `now`.
    fn restarted(&mut self, now: Instant) -> Option<HealthChange> {
        let change = self.reset(now);
        // The next pass records the container's new start time without a second reset.
        self.container = None;
        change
    }

    fn reset(&mut self, now: Instant) -> Option<HealthChange> {
        self.recovering |= self.liveness.stats().status == HealthStatus::Unhealthy;
        self.begin(now);
        self.liveness.reset(now)
    }

    fn begin(&mut self, now: Instant) {
        self.since = Some(now);
        self.passed = false;
        self.grace_open = true;
        self.down = false;
        self.next = now;
    }

    fn in_grace(&self, now: Instant) -> bool {
        self.grace_open && self.since.is_some_and(|since| now < since + self.grace)
    }

    /// When the probe after one at `now` is due: at the deploy interval within the grace.
    fn reschedule(&mut self, now: Instant, interval: Duration) {
        self.next = now
            + if self.in_grace(now) {
                self.interval
            } else {
                interval
            };
    }

    /// Whether the running container passed since its (re)start, as of `now`. The settle check
    /// passes by running for its period, so a container that has passes even before a monitor
    /// probe records it.
    fn readiness(&self, now: Instant) -> Readiness {
        let until = self.since.unwrap_or(now) + self.grace;
        if self.passed || (self.settle && now >= until) {
            Readiness::Passed
        } else {
            Readiness::Pending { until }
        }
    }

    /// Whether it still has to pass since its (re)start, which the reconciler's gate waits for.
    fn awaits_pass(&self) -> bool {
        !self.passed && self.since.is_some()
    }

    /// Notes a probe of a crash-looping generation at `now`, which says only whether it
    /// passed: the status stays as the reconciler set it. Returns whether it is the first
    /// pass since the (re)start.
    fn passes(&mut self, now: Instant, ok: bool) -> bool {
        let ran_long_enough = !self.settle || self.since.is_some_and(|s| now >= s + self.grace);
        if !ok || !ran_long_enough || !self.awaits_pass() {
            return false;
        }
        self.passed = true;
        self.grace_open = false;
        true
    }

    /// Records the probe `result` of a pass at `now`; the probe after a first pass is due at
    /// the monitor's `interval`.
    fn observe(
        &mut self,
        now: Instant,
        interval: Duration,
        result: std::result::Result<String, String>,
    ) -> Observed {
        if result.is_ok() && self.settle {
            // Running is passing only once it ran for the settle period (ADR-0056).
            let until = self.since.unwrap_or(now) + self.grace;
            if now < until {
                self.liveness.pending(result);
                self.next = self.next.min(until);
                return Observed::default();
            }
        }
        if result.is_ok() {
            let first_pass = !self.passed && self.since.is_some();
            if self.grace_open {
                self.next = now + interval;
            }
            self.passed = true;
            self.grace_open = false;
            let change = self.liveness.observe(now, result);
            let recovered = self.recovering
                && change
                    .as_ref()
                    .is_some_and(|c| c.from == HealthStatus::Unknown);
            if change.is_some() {
                self.recovering = false;
            }
            return Observed {
                change,
                first_pass,
                recovered,
            };
        }
        if self.in_grace(now) {
            self.liveness.pending(result);
            return Observed::default();
        }
        self.grace_open = false;
        let change = self.liveness.observe(now, result);
        if change.is_some() {
            self.recovering = false;
        }
        Observed {
            change,
            ..Observed::default()
        }
    }
}

/// Every watched generation.
pub(super) type Watchlist = BTreeMap<ServiceGeneration, Watched>;

/// One probe to carry out in a pass.
struct Job {
    key: ServiceGeneration,
    check: Result<Check, String>,
    /// The generation's container and its state, if it has one; an exec check runs in it.
    container: Option<(ContainerId, ContainerState)>,
    /// The monitor's interval for the service.
    interval: Duration,
    /// Only whether it passes counts: it is crash looping, and the status says so.
    pass_only: bool,
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
            let Some(definition) = self.committed_definition(&stack.name).await? else {
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
                let check = self.check_for(service, &pointer.ports);
                let interval = self.settings.monitor.interval_for(service);
                let restarts =
                    self.restart_stats(&stack.name, &pointer.service, pointer.generation);
                let found = containers
                    .iter()
                    .find(|c| ServiceGeneration::try_from(*c).as_ref() == Ok(&key));
                let due = {
                    let mut watchlist = self.watchlist();
                    let watched = watchlist
                        .entry(key.clone())
                        .or_insert_with(|| Watched::new(&check, now));
                    if let Some(change) = watched.see(found, now) {
                        events.push(UiEvent::health(&key, change));
                    }
                    if restarts.crash_looping {
                        let detail = format!(
                            "the reconciler restarted it {} times; crash looping",
                            restarts.restarts
                        );
                        if let Some(change) = watched.liveness.crash_looping(now, detail) {
                            recorded.extend(ServiceEvents::from(&key).health_changed(&change));
                            events.push(UiEvent::health(&key, change));
                        }
                        // Services waiting for it need to know when it passes; its status
                        // stays crash looping until the reconciler says otherwise. Only a
                        // running container can pass: while it is down it is looked at at the
                        // monitor's interval, and a restart makes it due at once.
                        let running = found.is_some_and(|c| c.state.is_running());
                        if watched.awaits_pass() && running && now >= watched.next {
                            watched.next = now + watched.interval;
                            Some(true)
                        } else {
                            if !(watched.awaits_pass() && running) {
                                watched.next = watched.next.max(now + interval);
                            }
                            None
                        }
                    } else if now >= watched.next {
                        watched.reschedule(now, interval);
                        Some(false)
                    } else {
                        None
                    }
                };
                if let Some(pass_only) = due {
                    jobs.push(Job {
                        key,
                        check: check.map_err(|err| err.to_string()),
                        container: found.map(|c| (c.id.clone(), c.state)),
                        interval,
                        pass_only,
                    });
                }
            }
        }

        let probes = Probes::new(&*self.runtime, &*self.prober, &self.execs);
        let results = join_all(jobs.iter().map(|job| job.run(&probes))).await;
        let mut wake = false;
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
                let Some(watched) = watchlist.get_mut(&job.key) else {
                    continue;
                };
                if job.pass_only {
                    wake |= watched.passes(now, result.is_ok());
                    continue;
                }
                let observed = watched.observe(now, job.interval, result);
                wake |= observed.first_pass;
                if let Some(change) = observed.change {
                    let service = ServiceEvents::from(&job.key);
                    if observed.recovered {
                        recorded.push(service.healthy_again(&change));
                    }
                    recorded.extend(service.health_changed(&change));
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
            self.emit_health(event.clone());
        }
        for event in recorded {
            self.events.record(event).await;
        }
        if wake {
            // Services waiting for one that just passed may start (ADR-0056).
            self.reconcile_wake.notify_one();
        }
        Ok(events)
    }

    /// The check of `service`'s current containers on their loopback `ports`, as deploys and
    /// the monitor run it: the "running for 5 s" check of a service without routes runs for
    /// [`Settings::settle`](super::Settings::settle).
    pub(super) fn check_for(
        &self,
        service: &Service,
        ports: &PortMap,
    ) -> std::result::Result<Check, HealthError> {
        let check = Check::for_service(service, ports)?;
        Ok(if check.is_settle() {
            Check::settle(self.settings.settle, self.settings.settle_poll)
        } else {
            check
        })
    }

    /// Hoster (re)started `key`'s container at `now`: its status resets to `Unknown` and its
    /// grace starts again (ADR-0056 Decision 3).
    pub(super) fn restarted(&self, key: &ServiceGeneration, now: Instant) {
        let change = self
            .watchlist()
            .get_mut(key)
            .and_then(|watched| watched.restarted(now));
        if let Some(change) = change {
            self.emit_health(UiEvent::health(key, change));
        }
    }

    /// Whether `container`, the running current container of `key` checked with `check`, has
    /// passed its check since hoster (re)started it or first saw it running: what the
    /// reconciler's gate reads (ADR-0056 Decision 3). A container the monitor has not seen yet
    /// is seen now.
    pub(super) fn readiness(
        &self,
        key: &ServiceGeneration,
        check: &std::result::Result<Check, HealthError>,
        container: &ContainerInfo,
        now: Instant,
    ) -> Readiness {
        let (readiness, change) = {
            let mut watchlist = self.watchlist();
            let watched = watchlist
                .entry(key.clone())
                .or_insert_with(|| Watched::new(check, now));
            let change = watched.see(Some(container), now);
            (watched.readiness(now), change)
        };
        if let Some(change) = change {
            self.emit_health(UiEvent::health(key, change));
        }
        readiness
    }

    /// Logs the health change `event` and sends it on `ui_events`.
    fn emit_health(&self, event: UiEvent) {
        event.log();
        self.emit(event);
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
