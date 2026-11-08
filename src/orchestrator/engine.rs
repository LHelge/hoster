//! The deploy task: drives one deployment through the pure machine and carries out its effects
//! against the [`ContainerRuntime`], the port allocator and the store.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;

use futures_util::future::try_join_all;
use tracing::Instrument;

use super::machine::{Effect, Event};
use super::plan::{Change, Plan, SecretVersions};
use super::{Orchestrator, Result};
use crate::health::Check;
use crate::logging::LogSpan;
use crate::routing::RouteClaims;
use crate::runtime::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerSpec, NetworkSpec, PortBinding,
    ServiceGeneration, VolumeMount, label,
};
use crate::stack::{Name, Stack, UpdateStrategy};
use crate::store::{
    DeploymentId, DeploymentState, DigestSet, PortMap, RevisionId, ServicePointer, StackRevision,
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
        }
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
    /// New containers created by this deployment.
    created: Vec<ContainerId>,
    /// Old stop-first containers `Replacing` stopped (kept until clean-up).
    stopped: Vec<ContainerId>,
}

type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

impl Run {
    /// A deployment whose record was created with the target digest set `recorded`.
    pub(super) fn new(id: DeploymentId, target: Target, recorded: DigestSet) -> Self {
        Self {
            id,
            target,
            recorded,
            state: DeploymentState::Received,
            plan: Err("not checked yet".into()),
            generation: 0,
            ports: BTreeMap::new(),
            created: Vec::new(),
            stopped: Vec::new(),
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
            if let Some(event) = self.cancelled(run, &effect) {
                tracing::info!(deployment = %run.id, "stack is being removed; aborting the deployment");
                return Some(event);
            }
            if let Some(event) = self.execute(run, effect).await {
                return Some(event);
            }
        }
        None
    }

    /// While the stack is being removed, the work of a state before the commit point is skipped
    /// and replaced by the event that ends the deployment: `Validating` rejects, the later
    /// states abort (which undoes what the deployment did). From `Switching` on the
    /// deployment runs to its end, since committed pointers cannot be undone (ADR-0048).
    fn cancelled(&self, run: &Run, effect: &Effect) -> Option<Event> {
        let reason = || super::STACK_REMOVED.to_owned();
        let event = match effect {
            Effect::Validate => Event::Invalid(reason()),
            Effect::PullImages
            | Effect::StartContainers
            | Effect::Probe
            | Effect::ReplaceStopFirst => Event::Failed(reason()),
            _ => return None,
        };
        self.is_removing(run.target.stack().as_ref())
            .then_some(event)
    }

    /// Runs `work` unless the stack's removal starts first; `None` when it did (`work` is
    /// dropped). Used for the waits a removal should not sit through: pulls and health checks.
    async fn unless_removed<T>(&self, run: &Run, work: impl Future<Output = T>) -> Option<T> {
        let stack = run.target.stack().as_ref();
        let mut work = pin!(work);
        loop {
            let mut removal = pin!(self.removal.notified());
            removal.as_mut().enable();
            if self.is_removing(stack) {
                return None;
            }
            tokio::select! {
                out = &mut work => return Some(out),
                () = &mut removal => {}
            }
        }
    }

    /// Applies the pure transition function and updates the in-memory slot.
    fn transition(
        &self,
        run: &mut Run,
        event: Event,
    ) -> std::result::Result<Vec<Effect>, super::machine::InvalidTransition> {
        let transition = run.state.on(event)?;
        // The store logs the persisted transition (target `hoster::deploy`) at `info`.
        tracing::debug!(deployment = %run.id, from = %run.state, to = %transition.to, "deployment transition");
        run.state = transition.to;
        let mut slots = self.lock();
        if let Some(slot) = slots.get_mut(run.target.stack().as_ref())
            && let Some((id, state)) = &mut slot.active
            && *id == run.id
        {
            *state = transition.to;
        }
        Ok(transition.effects)
    }

    /// The interpreter: carries out one effect.
    async fn execute(self: &Arc<Self>, run: &mut Run, effect: Effect) -> Option<Event> {
        match effect {
            Effect::Persist { to, message } => {
                if let Err(err) = self.store.transition(run.id, to, message.as_deref()).await {
                    tracing::error!(deployment = %run.id, %err, "cannot persist transition");
                }
                None
            }
            Effect::CheckRunning => Some(self.check_running(run).await),
            Effect::Validate => Some(self.validate(run).await),
            Effect::PullImages => Some(
                self.unless_removed(run, self.pull(run))
                    .await
                    .unwrap_or_else(|| Event::Failed(super::STACK_REMOVED.to_owned())),
            ),
            Effect::StartContainers => Some(self.start_new(run).await),
            Effect::Probe => Some(
                match self.unless_removed(run, self.settle(&run.created)).await {
                    Some(Ok(())) => Event::Healthy,
                    Some(Err(reason)) => Event::Failed(reason),
                    None => Event::Failed(super::STACK_REMOVED.to_owned()),
                },
            ),
            Effect::ReplaceStopFirst => Some(self.replace_stop_first(run).await),
            // M3: there is no proxy to publish a RouteTable to (§5.1).
            Effect::PublishRoutes => None,
            Effect::Commit => self.commit(run).await,
            // M3: nothing routes to the old upstreams, so there is nothing to drain.
            Effect::Drain => Some(Event::Drained),
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
        for image in images {
            if let Err(err) = self.runtime.pull_image(image).await {
                return Event::Failed(format!("cannot pull {image}: {err}"));
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
            match self.unless_removed(run, self.settle(&new)).await {
                Some(Ok(())) => {}
                Some(Err(reason)) => return Event::Failed(reason),
                None => return Event::Failed(super::STACK_REMOVED.to_owned()),
            }
        }
        Event::Replaced
    }

    /// `Switching`: the commit point, one transaction writing every changed service's pointer
    /// (§5.4). A failure here is fatal for the run: it halts in `Switching`, which crash
    /// recovery treats like `HealthChecking`.
    async fn commit(self: &Arc<Self>, run: &Run) -> Option<Event> {
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
        match self.store.commit(run.id, &pointers, &removed).await {
            Ok(()) => Some(Event::Switched),
            Err(err) => {
                tracing::error!(deployment = %run.id, %err, "cannot commit the per-service pointers; halting");
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
        for id in std::mem::take(&mut run.created) {
            if let Err(err) = self.runtime.remove(&id).await {
                problems.push(format!("cannot remove new container {id}: {err}"));
            }
        }
        for id in std::mem::take(&mut run.stopped) {
            if let Err(err) = self.runtime.start(&id).await {
                problems.push(format!("cannot restart old container {id}: {err}"));
            }
        }
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
            run.created.push(id.clone());
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

    /// The built-in health check (§5.1, M3): every container stays running for
    /// [`Settings::settle`](super::Settings::settle), checked concurrently.
    async fn settle(&self, ids: &[ContainerId]) -> Result<(), String> {
        let check = Check::settle(self.settings.settle, self.settings.settle_poll);
        let runtime = &*self.runtime;
        try_join_all(ids.iter().map(|id| check.run(runtime, id, &())))
            .await
            .map(drop)
            .map_err(|err| err.to_string())
    }
}

impl Run {
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
