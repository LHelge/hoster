//! The deploy task: drives one deployment through the pure machine and carries out its effects
//! against the [`ContainerRuntime`], the port allocator, the store and the published routes.
//!
//! The start-first path of ADR-0017 as ordered by ADR-0048: new start-first containers start next
//! to the old ones on fresh loopback ports (`Starting`) and are probed with their resolved
//! [`Check`]s (`HealthChecking`); stop-first services are replaced one at a time behind a 503 gap
//! (`Replacing`); one `RouteTable` publish and one pointer transaction commit the whole stack
//! (`Switching`); the old upstreams drain on their in-flight counters, bounded by the stack's
//! `drain_timeout` (`Draining`); then the old generation is removed (`CleaningUp`).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;

use futures_util::future::{join_all, try_join_all};
use tracing::Instrument;

use super::event_log::DeployEvents;
use super::machine::{Effect, Event};
use super::plan::{Change, Plan, SecretVersions};
use super::routes::Routes;
use super::{Interruption, Orchestrator, Result};
use crate::health::{Check, DeploymentLog};
use crate::logging::LogSpan;
use crate::routing::{DesiredStack, DrainOutcome, RouteClaims, ServiceState, Upstream};
use crate::runtime::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerSpec, NetworkSpec, PortBinding,
    ServiceGeneration, VolumeMount, label,
};
use crate::stack::{Name, Stack, UpdateStrategy};
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
    /// Old stop-first containers `Replacing` stopped (kept until clean-up).
    stopped: Vec<ContainerId>,
    /// What the stack's routes served before `Switching` published this deployment's: put
    /// back should the pointers fail to commit, and the old upstreams `Draining` waits for.
    served_before: Option<DesiredStack>,
    /// What the deployment writes to the event log (ADR-0055).
    events: DeployEvents,
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
            stopped: Vec::new(),
            served_before: None,
            events,
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
            Effect::Probe => Some(
                match self
                    .unless_interrupted(run, self.probe(run, &run.created))
                    .await
                {
                    Ok(Ok(())) => Event::Healthy,
                    Ok(Err(reason)) => Event::Failed(reason),
                    Err(interruption) => Event::Failed(interruption.to_string()),
                },
            ),
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

    /// `Starting`: the stack network, then every new start-first container.
    async fn start_new(&self, run: &mut Run) -> Event {
        let network = NetworkSpec::for_stack(run.target.stack());
        if let Err(err) = self.runtime.ensure_network(&network).await {
            return Event::Failed(format!("cannot create network {}: {err}", network.name));
        }
        let changes: Vec<Change> = run
            .plan()
            .changes_with(UpdateStrategy::StartFirst)
            .cloned()
            .collect();
        for change in &changes {
            if let Err(reason) = self.create_and_start(run, change).await {
                return Event::Failed(reason);
            }
        }
        Event::Started
    }

    /// `Replacing`: each stop-first service in turn (ADR-0048): stop the old container (keep
    /// it), start the new one, check it.
    async fn replace_stop_first(&self, run: &mut Run) -> Event {
        let changes: Vec<Change> = run
            .plan()
            .changes_with(UpdateStrategy::StopFirst)
            .cloned()
            .collect();
        if changes.is_empty() {
            return Event::Replaced;
        }
        let containers = match self.runtime.list_managed().await {
            Ok(containers) => containers,
            Err(err) => return Event::Failed(err.to_string()),
        };
        let definition = Arc::clone(&run.target.definition);
        let stop_timeout = |service: &Name| definition.services[service].stop_timeout;
        for change in &changes {
            if let Some(previous) = &change.previous {
                // The service's routes answer 503 + Retry-After until the commit or the abort.
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
                        return Event::Failed(format!("cannot stop {}: {err}", container.name));
                    }
                    run.stopped.push(container.id.clone());
                }
            }
            if let Err(reason) = self.create_and_start(run, change).await {
                return Event::Failed(reason);
            }
            let new = run.created.last().cloned().into_iter().collect::<Vec<_>>();
            match self.unless_interrupted(run, self.probe(run, &new)).await {
                Ok(Ok(())) => {}
                Ok(Err(reason)) => return Event::Failed(reason),
                Err(interruption) => return Event::Failed(interruption.to_string()),
            }
        }
        Event::Replaced
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

    /// `Aborting`: back to exactly how the stack was (ADR-0048).
    async fn abort(&self, run: &mut Run, reason: String) -> Event {
        let mut problems = Vec::new();
        for (_, id) in std::mem::take(&mut run.created) {
            if let Err(err) = self.runtime.remove(&id).await {
                problems.push(format!("cannot remove new container {id}: {err}"));
            }
        }
        for id in std::mem::take(&mut run.stopped) {
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
    async fn create_and_start(&self, run: &mut Run, change: &Change) -> Result<(), String> {
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
            Ok(())
        }
        .instrument(span)
        .await
    }

    /// `HealthChecking` and `Replacing`: probes the new containers `created` concurrently, each
    /// with its service's resolved health check (ADR-0038, ADR-0051) through the orchestrator's
    /// [`Prober`](crate::health::Prober). Probe results are recorded as events of the
    /// deployment; the first failure is the reason the deployment aborts.
    async fn probe(&self, run: &Run, created: &[(Name, ContainerId)]) -> Result<(), String> {
        let checks = created
            .iter()
            .map(|(service, id)| Ok((self.check(run, service)?, id)))
            .collect::<Result<Vec<_>, String>>()?;
        let log = DeploymentLog::new(self.store.clone(), run.id, run.state);
        let runtime = &*self.runtime;
        let prober = &*self.prober;
        try_join_all(
            checks
                .iter()
                .map(|(check, id)| check.run(runtime, id, prober, &log)),
        )
        .await
        .map(drop)
        .map_err(|err| err.to_string())
    }

    /// The health check of `service`'s new container on the ports this deployment reserved.
    /// The "running for 5 s" check of a service without routes runs for
    /// [`Settings::settle`](super::Settings::settle).
    fn check(&self, run: &Run, service: &Name) -> Result<Check, String> {
        let definition = &run.target.definition.services[service];
        let ports = run.ports.get(service).cloned().unwrap_or_default();
        let check = Check::for_service(definition, &ports)
            .map_err(|err| format!("service {service}: {err}"))?;
        Ok(if check.is_settle() {
            Check::settle(self.settings.settle, self.settings.settle_poll)
        } else {
            check
        })
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
            .changes_with(UpdateStrategy::StartFirst)
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
