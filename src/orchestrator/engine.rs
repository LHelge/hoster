//! The deploy task: drives one deployment through the pure machine and carries out its effects
//! against the [`ContainerRuntime`], the port allocator, the store and the published routes.
//!
//! The start-first path of ADR-0017 as ordered by ADR-0048 and ADR-0056: the start phase's new
//! containers start next to the old ones on fresh loopback ports (`Starting`), each as soon as
//! its start-phase dependencies have passed, and are probed in parallel with their resolved
//! [`Check`]s (`HealthChecking`); stop-first services are replaced one at a time in dependency
//! order behind a 503 gap, and services held back for them start once they have passed
//! (`Replacing`); one `RouteTable` publish and one pointer transaction commit the whole stack
//! (`Switching`); the old upstreams drain on their in-flight counters, bounded by the stack's
//! `drain_timeout` (`Draining`); then the old generation is removed (`CleaningUp`).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;

use futures_util::future::join_all;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::Instrument;

use super::event_log::DeployEvents;
use super::machine::{Effect, Event};
use super::notes::Note;
use super::plan::{Change, Phase, Plan, SecretVersions, StartWaves};
use super::routes::Routes;
use super::{Interruption, Orchestrator, Result};
use crate::health::{Check, DeploymentLog, ProbeLog, Probes};
use crate::logging::LogSpan;
use crate::routing::{DesiredStack, DrainOutcome, RouteClaims, ServiceState, Upstream};
use crate::runtime::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerSpec, NetworkSpec, PortBinding,
    ServiceGeneration, VolumeMount, label,
};
use crate::secrets::SecretValue;
use crate::stack::{Name, Stack};
use crate::store::{
    Deployment, DeploymentId, DeploymentState, DigestSet, NewEvent, PortMap, RevisionId,
    ServicePointer, StackRevision, Timestamp,
};

/// What a deployment wants to reach, before it is recorded.
#[derive(Debug, Clone)]
pub(super) struct Target {
    definition: Arc<Stack>,
    revision: StackRevision,
    /// The revision's pinned third-party digests (ADR-0025).
    pins: DigestSet,
    /// Digests asked for explicitly (webhook, poller, UI, rollback).
    requested: DigestSet,
    /// The earlier deployment a rollback redeploys (ADR-0041).
    rollback_of: Option<DeploymentId>,
}

impl Target {
    pub(super) fn new(
        definition: Arc<Stack>,
        revision: StackRevision,
        pins: DigestSet,
        requested: DigestSet,
    ) -> Self {
        Self {
            definition,
            revision,
            pins,
            requested,
            rollback_of: None,
        }
    }

    /// The target, as the rollback to the `earlier` deployment (ADR-0041).
    pub(super) fn rolling_back(mut self, earlier: DeploymentId) -> Self {
        self.rollback_of = Some(earlier);
        self
    }

    pub(super) fn stack(&self) -> &Name {
        &self.definition.name
    }

    pub(super) fn revision_id(&self) -> RevisionId {
        self.revision.id
    }

    pub(super) fn definition(&self) -> &Stack {
        &self.definition
    }

    /// Compares the target with what runs, given the current `secrets` versions.
    pub(super) fn plan(
        &self,
        running: &[ServicePointer],
        secrets: &SecretVersions,
    ) -> Result<Plan, super::PlanError> {
        Plan::new(
            &self.definition,
            &self.pins,
            &self.requested,
            running,
            secrets,
        )
    }
}

/// One deployment in flight: its target, where the machine is, and what it has done so far
/// (so `Aborting` and `CleaningUp` know what to undo or remove).
pub(super) struct Run {
    id: DeploymentId,
    target: Target,
    /// The target digest set the deployment's record holds.
    recorded: DigestSet,
    state: DeploymentState,
    /// Set by `CheckRunning`; a deployment without a plan is rejected in `Validating`.
    plan: std::result::Result<Plan, String>,
    generation: u32,
    /// Loopback ports reserved for the new containers, by service.
    ports: BTreeMap<Name, PortMap>,
    /// New containers created by this deployment, by service.
    created: Vec<(Name, ContainerId)>,
    /// The secret values given to the new containers, redacted from exec check output.
    secrets: Vec<SecretValue>,
    /// Old stop-first containers `Replacing` stopped (kept until clean-up), by service.
    stopped: Vec<(Name, ContainerId)>,
    /// The start-phase services `HealthChecking` has yet to start.
    waves: StartWaves,
    /// The unchanged services whose container ran when `Starting` began: restarted should one
    /// exit while a replaced dependency is down (ADR-0056 Decision 2.4).
    running_at_start: BTreeSet<Name>,
    /// What the stack's routes served before `Switching` published this deployment's: put
    /// back should the pointers fail to commit, and the old upstreams `Draining` waits for.
    served_before: Option<DesiredStack>,
    /// What the deployment writes to the event log (ADR-0055).
    events: DeployEvents,
    /// Held while a note is written, so notes of probes running in parallel and the deploy
    /// task's own are stored in the order of their timestamps.
    notes: Arc<tokio::sync::Mutex<()>>,
}

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

impl Run {
    /// A deployment of `target`, as its `record` was created.
    pub(super) fn new(target: Target, record: Deployment) -> Self {
        let events = DeployEvents::from(&record).rolling_back(target.rollback_of);
        Self {
            id: record.id,
            target,
            recorded: record.target,
            state: DeploymentState::Received,
            plan: Err("not checked yet".into()),
            generation: 0,
            ports: BTreeMap::new(),
            created: Vec::new(),
            secrets: Vec::new(),
            stopped: Vec::new(),
            waves: StartWaves::default(),
            running_at_start: BTreeSet::new(),
            served_before: None,
            events,
            notes: Arc::default(),
        }
    }

    pub(super) fn id(&self) -> DeploymentId {
        self.id
    }

    pub(super) fn state(&self) -> DeploymentState {
        self.state
    }

    fn stack_name(&self) -> String {
        self.target.stack().to_string()
    }

    /// The event log entry the move to `to` writes, if any (see [`DeployEvents::on`]): the
    /// services listed are the ones the plan changes, or the recorded target without one.
    fn event_on(&mut self, to: DeploymentState, message: Option<&str>) -> Option<NewEvent> {
        let services: Vec<(&str, &str)> = match &self.plan {
            Ok(plan) => plan
                .changes
                .iter()
                .map(|c| (c.service.as_ref(), c.image.digest().as_ref()))
                .collect(),
            Err(_) => self
                .recorded
                .iter()
                .map(|(service, digest)| (service.as_str(), digest.as_str()))
                .collect(),
        };
        self.events.on(to, message, services)
    }

    fn plan(&self) -> &Plan {
        // Only reached after `Validating` accepted the plan.
        self.plan
            .as_ref()
            .expect("validated deployments have a plan")
    }

    /// The failure of `service` because of `reason`, naming the changed services depending on
    /// it, even transitively, that were never started (ADR-0055, ADR-0056).
    fn failed(&self, service: &Name, reason: String) -> Event {
        let started: BTreeSet<&Name> = self.created.iter().map(|(s, _)| s).collect();
        let definition = &self.target.definition;
        let never: Vec<&str> = self
            .plan()
            .changes
            .iter()
            .filter(|c| {
                !started.contains(&c.service)
                    && definition
                        .transitive_dependencies(&c.service)
                        .contains(service)
            })
            .map(|c| c.service.as_ref())
            .collect();
        if never.is_empty() {
            Event::Failed(reason)
        } else {
            Event::Failed(format!("{reason}; never started: {}", never.join(", ")))
        }
    }
}

impl<R: ContainerRuntime> Orchestrator<R> {
    /// The deploy task: feeds `event` to the machine and keeps going until no effect produces
    /// another event (a terminal state, `Queued`, or a halted commit).
    ///
    /// The task runs in the deployment's [`LogSpan`], so everything it logs carries the stack,
    /// the deployment id and, once `Validating` assigned it, the generation.
    pub(super) fn drive(self: Arc<Self>, mut run: Run, event: Event) -> BoxFuture {
        let generation = (run.generation != 0).then_some(run.generation);
        let span = LogSpan::deployment(run.target.stack().as_ref(), run.id, generation);
        Box::pin(
            async move {
                let mut next = Some(event);
                while let Some(event) = next {
                    next = self.step(&mut run, event).await;
                }
            }
            .instrument(span),
        )
    }

    /// One transition: the pure machine under the lock (ADR-0050), then its effects in order.
    /// Returns the event the state's work produced, if any.
    pub(super) async fn step(self: &Arc<Self>, run: &mut Run, event: Event) -> Option<Event> {
        let effects = match self.transition(run, event) {
            Ok(effects) => effects,
            Err(err) => {
                tracing::error!(deployment = %run.id, %err, "deploy engine bug");
                self.release(&run.stack_name(), run.id);
                return None;
            }
        };
        for effect in effects {
            if let Some(event) = self.interrupted(run, &effect) {
                tracing::info!(deployment = %run.id, reason = %event, "deployment interrupted; aborting it");
                return Some(event);
            }
            if let Some(event) = self.execute(run, effect).await {
                return Some(event);
            }
        }
        None
    }

    /// While the stack is being removed or the deployment is cancelled, the work of a state
    /// before the commit point is skipped and replaced by the event that ends the deployment:
    /// the later states abort (which undoes what the deployment did), and a removal also
    /// rejects in `Validating`. A cancelled deployment validates first, so that it ends in
    /// `Aborting` as ADR-0040 says. From `Switching` on the deployment runs to its end, since
    /// committed pointers cannot be undone (ADR-0048).
    fn interrupted(&self, run: &Run, effect: &Effect) -> Option<Event> {
        let interruption = self.interruption(run.target.stack().as_ref(), run.id)?;
        match effect {
            Effect::Validate if interruption == Interruption::Removal => {
                Some(Event::Invalid(interruption.to_string()))
            }
            Effect::PullImages
            | Effect::StartContainers
            | Effect::Probe
            | Effect::ReplaceStopFirst => Some(Event::Failed(interruption.to_string())),
            _ => None,
        }
    }

    /// Runs `work` unless the stack's removal starts or the deployment is cancelled first;
    /// `Err` when one did (`work` is dropped). Used for the waits an interruption should not
    /// sit through: pulls, health checks and the drain.
    async fn unless_interrupted<T>(
        &self,
        run: &Run,
        work: impl Future<Output = T>,
    ) -> std::result::Result<T, Interruption> {
        let stack = run.target.stack().as_ref();
        let mut work = pin!(work);
        loop {
            let mut interrupt = pin!(self.interrupt.notified());
            interrupt.as_mut().enable();
            if let Some(interruption) = self.interruption(stack, run.id) {
                return Err(interruption);
            }
            tokio::select! {
                out = &mut work => return Ok(out),
                () = &mut interrupt => {}
            }
        }
    }

    /// Applies the pure transition function and updates the in-memory slot, under the lock.
    ///
    /// An interrupted deployment (ADR-0040) whose work in a state before the commit point
    /// succeeded fails instead, decided in the same critical section that records the new state:
    /// a cancel accepted before `Switching` is never lost to a deployment moving on into it.
    ///
    /// On `Switching` it also publishes the `RouteTable` in that same critical section
    /// (ADR-0035 #1, ADR-0050): every changed service on its new upstreams, removed services
    /// gone. [`Effect::PublishRoutes`] is carried out here and not returned; the
    /// [`Effect::Commit`] that writes the pointers follows it in the returned effects.
    fn transition(
        &self,
        run: &mut Run,
        event: Event,
    ) -> std::result::Result<Vec<Effect>, super::machine::InvalidTransition> {
        let mut state = self.lock();
        let slot = state.slots.get_mut(run.target.stack().as_ref());
        let event = match (&event, slot.as_ref().and_then(|s| s.interruption(run.id))) {
            (
                Event::Pulled | Event::Started | Event::Healthy | Event::Replaced,
                Some(interruption),
            ) => Event::Failed(interruption.to_string()),
            _ => event,
        };
        let transition = run.state.on(event)?;
        // The store logs the persisted transition (target `hoster::deploy`) at `info`.
        tracing::debug!(deployment = %run.id, from = %run.state, to = %transition.to, "deployment transition");
        run.state = transition.to;
        let mut effects = transition.effects;
        if let Some(slot) = slot
            && let Some((id, active)) = &mut slot.active
            && *id == run.id
        {
            *active = transition.to;
        }
        if effects.contains(&Effect::PublishRoutes) {
            effects.retain(|effect| *effect != Effect::PublishRoutes);
            if let Ok(plan) = &run.plan {
                let stack = run.target.stack();
                let live = plan.changes.iter().map(|change| {
                    let ports = run.ports.get(&change.service).cloned().unwrap_or_default();
                    let upstreams =
                        Routes::loopback(stack, &change.service, run.generation, &ports);
                    (change.service.clone(), upstreams)
                });
                let removed: Vec<Name> = plan
                    .removed
                    .iter()
                    .filter_map(|pointer| pointer.service.parse().ok())
                    .collect();
                run.served_before =
                    state
                        .routes
                        .commit(Arc::clone(&run.target.definition), live, &removed);
            }
        }
        Ok(effects)
    }

    /// The interpreter: carries out one effect.
    async fn execute(self: &Arc<Self>, run: &mut Run, effect: Effect) -> Option<Event> {
        match effect {
            Effect::Persist { to, message } => {
                let event = run.event_on(to, message.as_deref());
                let _writing = Arc::clone(&run.notes).lock_owned().await;
                if let Err(err) = self
                    .persist(&run.stack_name(), run.id, to, message.as_deref(), event)
                    .await
                {
                    tracing::error!(deployment = %run.id, %err, "cannot persist transition");
                }
                None
            }
            Effect::CheckRunning => Some(self.check_running(run).await),
            Effect::Validate => Some(self.validate(run).await),
            Effect::PullImages => Some(
                self.unless_interrupted(run, self.pull(run))
                    .await
                    .unwrap_or_else(|interruption| Event::Failed(interruption.to_string())),
            ),
            Effect::StartContainers => Some(self.start_new(run).await),
            Effect::Probe => Some(self.start_waves(run).await),
            Effect::ReplaceStopFirst => Some(self.replace_stop_first(run).await),
            // Published by `transition`, under the lock; never returned from it.
            Effect::PublishRoutes => None,
            Effect::Commit => self.commit(run).await,
            // A removal takes every container of the stack down anyway, so it does not wait
            // for the old upstreams to go idle.
            Effect::Drain => Some(
                self.unless_interrupted(run, self.drain(run))
                    .await
                    .unwrap_or(Event::Drained),
            ),
            Effect::CleanUp => Some(self.clean_up(run).await),
            Effect::Abort { reason } => Some(self.abort(run, reason).await),
            Effect::Finish => {
                tracing::info!(deployment = %run.id, state = %run.state, "deployment finished");
                self.release(&run.stack_name(), run.id);
                None
            }
        }
    }

    /// `Received`: (re)compares the target with the current pointers (§5.2). A queued
    /// deployment is re-planned against what runs by now, so its record is brought up to date.
    pub(super) async fn check_running(&self, run: &mut Run) -> Event {
        let plan = match (
            self.store.services(&run.stack_name()).await,
            self.secret_versions(&run.target.definition).await,
        ) {
            (Ok(running), Ok(secrets)) => run
                .target
                .plan(&running, &secrets)
                .map_err(|e| e.to_string()),
            (Err(err), _) => Err(err.to_string()),
            (_, Err(err)) => Err(err.to_string()),
        };
        run.plan = plan;
        if let Ok(plan) = &run.plan {
            let target = plan.target();
            if target != run.recorded {
                match self.store.set_target(run.id, &target).await {
                    Ok(()) => run.recorded = target,
                    Err(err) => {
                        tracing::error!(deployment = %run.id, %err, "cannot record the target")
                    }
                }
            }
        }
        match &run.plan {
            Ok(plan) if plan.is_noop() => Event::Unchanged,
            _ => Event::Accepted,
        }
    }

    /// `Validating`: re-validates the revision and its routes, assigns the generation and
    /// reserves the new containers' loopback ports.
    async fn validate(&self, run: &mut Run) -> Event {
        if let Err(reason) = &run.plan {
            return Event::Invalid(reason.clone());
        }
        if let Err(err) = run.target.revision.toml.parse::<Stack>() {
            return Event::Invalid(err.to_string());
        }
        let others = match self.other_stacks(run.target.stack()).await {
            Ok(others) => others,
            Err(err) => return Event::Invalid(err.to_string()),
        };
        if let Err(err) =
            RouteClaims::check(&run.target.definition, &others, &self.settings.reserved)
        {
            return Event::Invalid(err.to_string());
        }
        if let Err(reason) = self.check_secrets(run).await {
            return Event::Invalid(reason);
        }
        run.generation = match self.store.assign_generation(run.id).await {
            Ok(generation) => generation,
            Err(err) => return Event::Invalid(err.to_string()),
        };
        tracing::Span::current().record("generation", run.generation);
        let stack = run.stack_name();
        let wanted: Vec<(Name, Vec<u16>)> = run
            .plan()
            .changes
            .iter()
            .map(|c| {
                let ports = c.container_ports(&run.target.definition);
                (c.service.clone(), ports.into_iter().collect())
            })
            .collect();
        for (service, container_ports) in wanted {
            for container_port in container_ports {
                match self
                    .ports
                    .allocate(&stack, service.as_ref(), container_port, run.generation)
                    .await
                {
                    Ok(port) => {
                        run.ports
                            .entry(service.clone())
                            .or_default()
                            .insert(container_port, port);
                    }
                    Err(err) => {
                        self.release_ports(run).await;
                        return Event::Invalid(err.to_string());
                    }
                }
            }
        }
        Event::Validated
    }

    /// Every secret a changed service references must be set (ADR-0029). The message names the
    /// secret, never a value.
    async fn check_secrets(&self, run: &Run) -> Result<(), String> {
        for change in &run.plan().changes {
            let service = &run.target.definition.services[&change.service];
            for (env, secret) in &service.secrets {
                match self.secrets.version(secret).await {
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        return Err(format!(
                            "service {}: secret `{secret}` (for {env}) is not set",
                            change.service
                        ));
                    }
                    Err(err) => return Err(format!("service {}: {err}", change.service)),
                }
            }
        }
        Ok(())
    }

    /// `Pulling`: every changed image, by digest.
    async fn pull(&self, run: &Run) -> Event {
        let mut images: Vec<_> = run.plan().changes.iter().map(|c| &c.image).collect();
        images.dedup();
        // Image GC removes an image only while it holds this exclusively and no deployment is
        // active, so an image this deployment finds present stays until it is done: GC sees the
        // deployment active from now on.
        let _pulling = self.images.read().await;
        for image in images {
            // Every stack shares the pull limit (ADR-0040); the wait for a permit is part of the
            // pull, so an interruption ends it too.
            let _permit = self
                .pulls
                .acquire()
                .await
                .expect("the pull semaphore is never closed");
            if let Err(err) = self.runtime.pull_image(image).await {
                return Event::Failed(format!("cannot pull {image}: {err}"));
            }
            // Image GC prunes only what this table records (ADR-0041). Failing to record just
            // means the image is never pruned, so the deployment carries on.
            let repository = format!("{}/{}", image.registry(), image.repository());
            let digest = image.digest().to_string();
            if let Err(err) = self.store.record_image(&repository, &digest).await {
                tracing::warn!(%image, %err, "cannot record the pulled image; it will not be pruned");
            }
        }
        Event::Pulled
    }

    /// `Starting`: the stack network, then the start phase's first wave: every start-phase
    /// service with no start-phase dependency (ADR-0056 Decision 2). Records which unchanged
    /// services run, and fails when an unchanged dependency of a changed service does not.
    async fn start_new(&self, run: &mut Run) -> Event {
        let network = NetworkSpec::for_stack(run.target.stack());
        if let Err(err) = self.runtime.ensure_network(&network).await {
            return Event::Failed(format!("cannot create network {}: {err}", network.name));
        }
        let containers = match self.runtime.list_managed().await {
            Ok(containers) => containers,
            Err(err) => return Event::Failed(err.to_string()),
        };
        let stack = run.target.stack().clone();
        run.running_at_start = run
            .plan()
            .unchanged
            .iter()
            .filter(|p| is_running(&containers, &stack, p))
            .filter_map(|p| p.service.parse().ok())
            .collect();
        let mut notes = Vec::new();
        let mut unchanged = BTreeMap::new();
        for change in &run.plan().changes {
            for dependency in change
                .depends_on
                .iter()
                .filter(|d| run.plan().is_awaiting(d))
            {
                notes.push(
                    Note::NeverRun {
                        dependency,
                        service: &change.service,
                    }
                    .to_string(),
                );
            }
            for pointer in run.plan().unchanged_dependencies(change) {
                if let Ok(dependency) = pointer.service.parse::<Name>() {
                    unchanged.insert(dependency, pointer.generation);
                }
            }
        }
        for (dependency, generation) in &unchanged {
            if !run.running_at_start.contains(dependency) {
                return run.failed(
                    dependency,
                    format!("{dependency} (unchanged) is not running"),
                );
            }
            let status = self
                .health_stats(stack.as_ref(), dependency.as_ref(), *generation)
                .status;
            notes.push(Note::UnchangedRunning { dependency, status }.to_string());
        }
        for note in notes {
            self.note_run(run, &note).await;
        }
        run.waves = run.plan().start_waves();
        let ready = run.waves.ready();
        if let Err(event) = self.start_wave(run, &ready, None).await {
            return event;
        }
        Event::Started
    }

    /// Creates and starts the start-phase services `ready` (once `after` passed, if given),
    /// one at a time, each after an interruption check. Returns the containers started.
    async fn start_wave(
        &self,
        run: &mut Run,
        ready: &[Name],
        after: Option<(&Name, std::time::Duration)>,
    ) -> std::result::Result<Vec<(Name, ContainerId)>, Event> {
        let names: Vec<&str> = ready.iter().map(AsRef::as_ref).collect();
        if after.is_some() || !names.is_empty() {
            let note = Note::Wave {
                passed: after,
                starting: &names,
            };
            self.note_run(run, &note.to_string()).await;
        }
        let mut started = Vec::new();
        for service in ready {
            if let Some(interruption) = self.interruption(run.target.stack().as_ref(), run.id) {
                return Err(Event::Failed(interruption.to_string()));
            }
            let change = run
                .plan()
                .changes
                .iter()
                .find(|c| c.service == *service)
                .cloned()
                .expect("a start wave holds changed services");
            match self.create_and_start(run, &change).await {
                Ok(id) => started.push((service.clone(), id)),
                Err(reason) => return Err(run.failed(service, reason)),
            }
        }
        Ok(started)
    }

    /// `HealthChecking`: probes the started start-phase containers in parallel, and starts
    /// each further start-phase service as soon as its start-phase dependencies have passed,
    /// probing it too (ADR-0056 Decision 2.1). Held-back services are left to `Replacing`.
    /// The probes run as their own tasks, so they go on while a dependent is created; only the
    /// probe waits end on an interruption (dropping the set aborts them), and a create in
    /// progress finishes first.
    async fn start_waves(self: &Arc<Self>, run: &mut Run) -> Event {
        let mut pending = JoinSet::new();
        // Which service each probe task checks, so a probe task that panicked still names it.
        let mut probing = BTreeMap::new();
        let mut started = run.created.clone();
        loop {
            for (service, id) in started.drain(..) {
                let check = match self.check(run, &service) {
                    Ok(check) => check,
                    Err(reason) => return run.failed(&service, reason),
                };
                let log = self.probe_log(run);
                log.record(
                    &Note::Checking {
                        service: &service,
                        check: &check,
                    }
                    .to_string(),
                )
                .await;
                let orchestrator = Arc::clone(self);
                let probed = service.clone();
                let task = pending.spawn(
                    async move {
                        let probes = Probes::new(
                            &*orchestrator.runtime,
                            &*orchestrator.prober,
                            &orchestrator.execs,
                        );
                        let at = Instant::now();
                        let result = check.run(&probes, &id, &log).await;
                        (service, result.map_err(|err| err.to_string()), at.elapsed())
                    }
                    .in_current_span(),
                );
                probing.insert(task.id(), probed);
            }
            let (service, result, took) =
                match self.unless_interrupted(run, pending.join_next()).await {
                    Ok(Some(Ok(next))) => next,
                    Ok(Some(Err(err))) => {
                        let reason = format!("a probe failed: {err}");
                        return match probing.get(&err.id()) {
                            Some(service) => run.failed(service, reason),
                            None => Event::Failed(reason),
                        };
                    }
                    Ok(None) => return Event::Healthy,
                    Err(interruption) => return Event::Failed(interruption.to_string()),
                };
            if let Err(err) = result {
                return run.failed(&service, format!("{service}: {err}"));
            }
            run.waves.passed(&service);
            let ready = run.waves.ready();
            match self.start_wave(run, &ready, Some((&service, took))).await {
                Ok(new) => started = new,
                Err(event) => return event,
            }
        }
    }

    /// `Replacing` (ADR-0048, ADR-0056): one at a time in dependency order, each replace-phase
    /// service is replaced (stop the old container and keep it, start the new one, check it)
    /// and each held-back service started and checked, after re-checking that its unchanged
    /// dependencies run. After a replacement passes, the unchanged services depending on it
    /// that ran when the deployment began and have exited since are started again and checked.
    async fn replace_stop_first(&self, run: &mut Run) -> Event {
        let changes: Vec<Change> = run.plan().replacing().cloned().collect();
        let definition = Arc::clone(&run.target.definition);
        let stop_timeout = |service: &Name| definition.services[service].stop_timeout;
        for change in &changes {
            let containers = match self.runtime.list_managed().await {
                Ok(containers) => containers,
                Err(err) => return run.failed(&change.service, err.to_string()),
            };
            if let Some(dependency) = self.stopped_unchanged_dependency(run, change, &containers) {
                return run.failed(
                    &dependency,
                    format!("{dependency} (unchanged) is not running"),
                );
            }
            match (&change.phase, &change.previous) {
                (Phase::Replace, Some(previous)) => {
                    self.note_run(run, &Note::Replacing(&change.service).to_string())
                        .await;
                    // The service's routes answer 503 + Retry-After until the commit or the
                    // abort.
                    self.lock()
                        .routes
                        .open_gap(run.target.stack(), &change.service);
                    let old = generation_of(&containers, run.target.stack(), previous);
                    for container in old.into_iter().filter(|c| c.state.is_running()) {
                        if let Err(err) = self
                            .runtime
                            .stop(&container.id, stop_timeout(&change.service))
                            .await
                        {
                            let reason = format!("cannot stop {}: {err}", container.name);
                            return run.failed(&change.service, reason);
                        }
                        run.stopped
                            .push((change.service.clone(), container.id.clone()));
                    }
                }
                _ => {
                    let replaced: Vec<&str> = run
                        .plan()
                        .held_back_for(&change.service)
                        .map(AsRef::as_ref)
                        .collect();
                    let note = Note::HeldBack {
                        service: &change.service,
                        behind: &replaced,
                    };
                    self.note_run(run, &note.to_string()).await;
                }
            }
            let id = match self.create_and_start(run, change).await {
                Ok(id) => id,
                Err(reason) => return run.failed(&change.service, reason),
            };
            let check = match self.check(run, &change.service) {
                Ok(check) => check,
                Err(reason) => return run.failed(&change.service, reason),
            };
            let label = change.service.as_ref();
            if let Err(reason) = self
                .probe_one(run, &change.service, label, &id, check)
                .await
            {
                return run.failed(&change.service, reason);
            }
            if change.phase == Phase::Replace
                && let Err((service, reason)) = self
                    .restart_unchanged_dependents(run, &change.service)
                    .await
            {
                return run.failed(&service, reason);
            }
        }
        Event::Replaced
    }

    /// An unchanged service `change` depends on that does not run, if one does not.
    fn stopped_unchanged_dependency(
        &self,
        run: &Run,
        change: &Change,
        containers: &[ContainerInfo],
    ) -> Option<Name> {
        let stack = run.target.stack();
        run.plan()
            .unchanged_dependencies(change)
            .find(|p| !is_running(containers, stack, p))
            .and_then(|p| p.service.parse().ok())
    }

    /// After `replaced` passed: starts again, in dependency order, every unchanged service
    /// depending on it (even transitively) that ran when the deployment began and has exited
    /// since, without a restart record, and checks each within its `start_period` before the
    /// next (ADR-0056 Decision 2.4). The secret values it references are redacted from its
    /// check's output like those of the new containers. `Err` names the service that did not
    /// pass, and why.
    async fn restart_unchanged_dependents(
        &self,
        run: &mut Run,
        replaced: &Name,
    ) -> std::result::Result<(), (Name, String)> {
        let definition = Arc::clone(&run.target.definition);
        let mut dependents: Vec<(Name, ServicePointer)> = run
            .running_at_start
            .iter()
            .filter(|s| definition.transitive_dependencies(s).contains(replaced))
            .map(|s| {
                let pointer = run
                    .plan()
                    .unchanged_pointer(s)
                    .expect("running_at_start holds unchanged services");
                (s.clone(), pointer.clone())
            })
            .collect();
        if dependents.is_empty() {
            return Ok(());
        }
        definition.sort_in_dependency_order(&mut dependents, |(s, _)| s);
        let containers = self
            .runtime
            .list_managed()
            .await
            .map_err(|err| (replaced.clone(), err.to_string()))?;
        let stack = run.target.stack().clone();
        for (service, pointer) in &dependents {
            let Some(container) = generation_of(&containers, &stack, pointer)
                .into_iter()
                .find(|c| !c.state.is_running())
            else {
                continue;
            };
            let note = Note::Restarting { service, replaced };
            self.note_run(run, &note.to_string()).await;
            self.runtime.start(&container.id).await.map_err(|err| {
                (
                    service.clone(),
                    format!("cannot start {}: {err}", container.name),
                )
            })?;
            if let Ok(key) = ServiceGeneration::try_from(container) {
                self.restarted(&key, Instant::now());
            }
            for (env, secret) in &definition.services[service].secrets {
                match self.secrets.reveal(secret).await {
                    Ok(Some((value, _))) => run.secrets.push(value),
                    Ok(None) => {}
                    Err(err) => {
                        return Err((
                            service.clone(),
                            format!(
                                "{service} (unchanged): cannot read secret `{secret}` (for {env}): {err}"
                            ),
                        ));
                    }
                }
            }
            let check = self
                .check_for(&definition.services[service], &pointer.ports)
                .map_err(|err| (service.clone(), format!("service {service}: {err}")))?;
            let label = format!("{service} (unchanged)");
            self.probe_one(run, service, &label, &container.id, check)
                .await
                .map_err(|reason| (service.clone(), reason))?;
        }
        Ok(())
    }

    /// Probes `service`'s container `id` with `check` until it passes or fails, recording it
    /// as events of the deployment; ends early on an interruption. A failure is reported as
    /// `<label>: <why>`.
    async fn probe_one(
        &self,
        run: &Run,
        service: &Name,
        label: &str,
        id: &ContainerId,
        check: Check,
    ) -> std::result::Result<(), String> {
        let log = self.probe_log(run);
        log.record(
            &Note::Checking {
                service,
                check: &check,
            }
            .to_string(),
        )
        .await;
        let probes = Probes::new(&*self.runtime, &*self.prober, &self.execs);
        match self
            .unless_interrupted(run, check.run(&probes, id, &log))
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(format!("{label}: {err}")),
            Err(interruption) => Err(interruption.to_string()),
        }
    }

    /// Where probe results go: events of the deployment in its state, with the secret values
    /// of its containers redacted from exec output.
    fn probe_log(&self, run: &Run) -> DeploymentLog {
        DeploymentLog::new(self.store.clone(), run.id, run.state)
            .redacting(&run.secrets)
            .serialized(&run.notes)
    }

    /// Records `message` as an event of the deployment in its state.
    async fn note_run(&self, run: &Run, message: &str) {
        let _writing = run.notes.lock().await;
        if let Err(err) = self
            .note(&run.stack_name(), run.id, run.state, message)
            .await
        {
            tracing::warn!(deployment = %run.id, %err, "cannot record a deployment note");
        }
    }

    /// `Switching`: the commit point, one transaction writing every changed service's pointer
    /// (§5.4). A failure here is fatal for the run: it halts in `Switching`, which crash
    /// recovery treats like `HealthChecking`. The old pointers stay authoritative, and the
    /// reconciler retires the new generation, so the routes go back to the old upstreams.
    async fn commit(self: &Arc<Self>, run: &mut Run) -> Option<Event> {
        let pointers: Vec<ServicePointer> = run
            .plan()
            .changes
            .iter()
            .map(|c| {
                let ports = run.ports.get(&c.service).cloned().unwrap_or_default();
                c.pointer(run.generation, ports)
            })
            .collect();
        let removed: Vec<String> = run
            .plan()
            .removed
            .iter()
            .map(|p| p.service.clone())
            .collect();
        let succeeded = run.events.succeeded(Timestamp::now());
        match self
            .store
            .commit_recorded(run.id, &pointers, &removed, &succeeded)
            .await
        {
            Ok(event) => {
                self.events.announce(event);
                Some(Event::Switched)
            }
            Err(err) => {
                tracing::error!(deployment = %run.id, %err, "cannot commit the per-service pointers; halting");
                self.lock()
                    .routes
                    .revert(run.target.stack(), run.served_before.take());
                self.release(&run.stack_name(), run.id);
                None
            }
        }
    }

    /// `CleaningUp`: the previous generation of every changed service and every removed
    /// service goes, with its ports.
    async fn clean_up(&self, run: &mut Run) -> Event {
        let containers = match self.runtime.list_managed().await {
            Ok(containers) => containers,
            Err(err) => return Event::CleanupFailed(err.to_string()),
        };
        let stack = run.target.stack().clone();
        let plan = run.plan();
        let old: Vec<(&ServicePointer, std::time::Duration)> = plan
            .changes
            .iter()
            .filter_map(|c| {
                let timeout = run.target.definition.services[&c.service].stop_timeout;
                c.previous.as_ref().map(|p| (p, timeout))
            })
            .chain(
                plan.removed
                    .iter()
                    .map(|p| (p, crate::stack::DEFAULT_STOP_TIMEOUT)),
            )
            .collect();
        let mut problems = Vec::new();
        for (pointer, timeout) in old {
            for container in generation_of(&containers, &stack, pointer) {
                if container.state.is_running()
                    && let Err(err) = self.runtime.stop(&container.id, timeout).await
                {
                    problems.push(format!("cannot stop {}: {err}", container.name));
                }
                if let Err(err) = self.runtime.remove(&container.id).await {
                    problems.push(format!("cannot remove {}: {err}", container.name));
                }
            }
            for port in pointer.ports.values() {
                if let Err(err) = self.ports.release(*port).await {
                    problems.push(format!("cannot release port {port}: {err}"));
                }
            }
        }
        run.stopped.clear();
        if problems.is_empty() {
            Event::CleanedUp
        } else {
            Event::CleanupFailed(problems.join("; "))
        }
    }

    /// `Aborting`: back to exactly how the stack was (ADR-0048). The new containers go in
    /// reverse creation order; the stopped stop-first containers start again in the dependency
    /// order of the last committed deployment's revision, order only: nothing waits for a
    /// container to run or pass, and what does not come up is left to the reconciler
    /// (ADR-0056 Decision 2.6).
    async fn abort(&self, run: &mut Run, reason: String) -> Event {
        let mut problems = Vec::new();
        for (_, id) in std::mem::take(&mut run.created).into_iter().rev() {
            if let Err(err) = self.runtime.remove(&id).await {
                problems.push(format!("cannot remove new container {id}: {err}"));
            }
        }
        let mut stopped = std::mem::take(&mut run.stopped);
        if stopped.len() > 1 {
            let committed = match self.committed_definition(&run.stack_name()).await {
                Ok(Some(definition)) => definition,
                Ok(None) => Arc::clone(&run.target.definition),
                Err(err) => {
                    tracing::warn!(
                        deployment = %run.id, %err,
                        "cannot read the committed revision; restarting in the target's order"
                    );
                    Arc::clone(&run.target.definition)
                }
            };
            committed.sort_in_dependency_order(&mut stopped, |(service, _)| service);
        }
        for (_, id) in stopped {
            if let Err(err) = self.runtime.start(&id).await {
                problems.push(format!("cannot restart old container {id}: {err}"));
            }
        }
        // The stopped stop-first services serve from their old upstreams again.
        self.lock().routes.close_gaps(run.target.stack());
        self.release_ports(run).await;
        if problems.is_empty() {
            Event::Aborted(reason)
        } else {
            Event::Aborted(format!("{reason} (abort: {})", problems.join("; ")))
        }
    }

    async fn release_ports(&self, run: &mut Run) {
        for port in std::mem::take(&mut run.ports)
            .into_values()
            .flat_map(|m| m.into_values())
        {
            if let Err(err) = self.ports.release(port).await {
                tracing::warn!(port, %err, "cannot release reserved port");
            }
        }
    }

    /// Creates and starts the new container of `change`; it is recorded in `run.created`
    /// as soon as it exists, so an abort removes it.
    ///
    /// Secrets are read from the store here, at create time, and go into the spec's redacted
    /// `secrets` map only. Volumes are created first if they do not exist yet (ADR-0030).
    async fn create_and_start(
        &self,
        run: &mut Run,
        change: &Change,
    ) -> Result<ContainerId, String> {
        let span = LogSpan::service(
            run.target.stack().as_ref(),
            change.service.as_ref(),
            run.generation,
        );
        async {
            let mut spec = run.spec(change)?;
            let service = &run.target.definition.services[&change.service];
            for (env, secret) in &service.secrets {
                match self.secrets.reveal(secret).await {
                    Ok(Some((value, _))) => {
                        run.secrets.push(value.clone());
                        spec.secrets.insert(env.clone(), value);
                    }
                    Ok(None) => {
                        return Err(format!(
                            "cannot create {}: secret `{secret}` (for {env}) is not set",
                            spec.name
                        ));
                    }
                    Err(err) => return Err(format!("cannot create {}: {err}", spec.name)),
                }
            }
            for volume in &spec.volumes {
                self.runtime
                    .ensure_volume(volume.spec())
                    .await
                    .map_err(|err| format!("cannot create volume {}: {err}", volume.volume()))?;
            }
            let id = self
                .runtime
                .create_container(&spec)
                .await
                .map_err(|err| format!("cannot create {}: {err}", spec.name))?;
            run.created.push((change.service.clone(), id.clone()));
            self.runtime
                .start(&id)
                .await
                .map_err(|err| format!("cannot start {}: {err}", spec.name))?;
            tracing::info!(container = %spec.name, image = %change.image, "container started");
            Ok(id)
        }
        .instrument(span)
        .await
    }

    /// The health check of `service`'s new container on the ports this deployment reserved.
    /// The "running for 5 s" check of a service without routes runs for
    /// [`Settings::settle`](super::Settings::settle).
    fn check(&self, run: &Run, service: &Name) -> Result<Check, String> {
        let definition = &run.target.definition.services[service];
        let ports = run.ports.get(service).cloned().unwrap_or_default();
        self.check_for(definition, &ports)
            .map_err(|err| format!("service {service}: {err}"))
    }

    /// `Draining` (§5.3): waits for every upstream the swap took out of the table (the old
    /// generation of each changed service and the upstreams of removed services) to finish
    /// its in-flight requests. Each waits the settle delay first (ADR-0035 #2) and is bounded
    /// by the stack's `drain_timeout`; on the timeout the deployment proceeds and the
    /// requests still in flight are cut when the old container stops. Nothing to drain (a
    /// first deployment, unrouted or stop-first services only) completes at once.
    async fn drain(&self, run: &Run) -> Event {
        let old = run.old_upstreams();
        if old.is_empty() {
            return Event::Drained;
        }
        let settle = self.settings.drain_settle;
        let timeout = run.target.definition.drain_timeout;
        let outcomes = join_all(old.iter().map(|upstream| upstream.drain(settle, timeout))).await;
        for (upstream, outcome) in old.iter().zip(outcomes) {
            if let DrainOutcome::TimedOut(left) = outcome {
                let message = format!(
                    "drain of service {} g{} ({}) timed out after {} with {left} request(s) \
                     in flight; they are cut",
                    upstream.service,
                    upstream.generation,
                    upstream.addr,
                    humantime::format_duration(timeout),
                );
                if let Err(err) = self
                    .note(&run.stack_name(), run.id, run.state, &message)
                    .await
                {
                    tracing::warn!(deployment = %run.id, %err, "cannot record the drain timeout");
                }
            }
        }
        Event::Drained
    }
}

impl Run {
    /// The upstreams the swap took out of the table: what the stack served before it for every
    /// changed start-first or removed service. A stop-first service has nothing to drain: its
    /// old container stopped when its gap opened.
    fn old_upstreams(&self) -> Vec<Arc<Upstream>> {
        let (Some(before), Ok(plan)) = (&self.served_before, &self.plan) else {
            return Vec::new();
        };
        let retired: Vec<&str> = plan
            .changes
            .iter()
            .filter(|c| c.phase != Phase::Replace)
            .map(|c| c.service.as_ref())
            .chain(plan.removed.iter().map(|p| p.service.as_str()))
            .collect();
        before
            .services
            .iter()
            .filter(|(service, _)| retired.contains(&service.as_ref()))
            .flat_map(|(_, state)| match state {
                ServiceState::Live { upstreams } => upstreams.values().cloned().collect(),
                ServiceState::Gap { .. } => Vec::new(),
            })
            .collect()
    }

    /// The container spec of `change`'s new generation (architecture §8).
    fn spec(&self, change: &Change) -> Result<ContainerSpec, String> {
        let stack = self.target.stack();
        let service = &self.target.definition.services[&change.service];
        let generation = ServiceGeneration {
            stack: stack.clone(),
            service: change.service.clone(),
            generation: self.generation,
        };
        let mut spec = ContainerSpec::new(&generation, self.id, change.image.clone());
        spec.command = service.command.clone();
        spec.entrypoint = service.entrypoint.clone();
        // `env` plus the injected `PORT` (ADR-0051), exactly what the config hash covers.
        spec.env = service.container_env();
        spec.user = service.user.clone();
        spec.read_only = service.read_only;
        spec.stop_timeout = service.stop_timeout;
        let ports = self.ports.get(&change.service).cloned().unwrap_or_default();
        spec.ports = ports
            .into_iter()
            .map(|(container_port, host_port)| PortBinding {
                container_port,
                host_port,
            })
            .collect();
        spec.volumes = service
            .volumes
            .iter()
            .map(|(volume, target)| VolumeMount::named(stack, volume, target))
            .collect::<Result<_, _>>()?;
        Ok(spec)
    }
}

/// Whether a container of `pointer`'s generation of its service in `stack` runs.
fn is_running(containers: &[ContainerInfo], stack: &Name, pointer: &ServicePointer) -> bool {
    generation_of(containers, stack, pointer)
        .iter()
        .any(|c| c.state.is_running())
}

/// The containers of `pointer`'s generation of its service in `stack`.
fn generation_of<'a>(
    containers: &'a [ContainerInfo],
    stack: &Name,
    pointer: &ServicePointer,
) -> Vec<&'a ContainerInfo> {
    let generation = pointer.generation.to_string();
    containers
        .iter()
        .filter(|c| {
            c.labels.get(label::STACK).map(String::as_str) == Some(stack.as_ref())
                && c.labels.get(label::SERVICE) == Some(&pointer.service)
                && c.labels.get(label::GENERATION) == Some(&generation)
        })
        .collect()
}
