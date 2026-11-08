//! The `Orchestrator` handle (ADR-0050): deploy state machine, per-stack deploy tasks and the
//! reconciler that keeps containers running (ADR-0028).
//!
//! [`Orchestrator`] is the single writer of desired state. Callers (the web UI, the webhook, the
//! poller) hold an `Arc<Orchestrator>` and call its async methods: [`apply`](Orchestrator::apply),
//! [`deploy`](Orchestrator::deploy), [`remove`](Orchestrator::remove) (see [`RemoveStack`]),
//! [`rollback`](Orchestrator::rollback) and [`status`](Orchestrator::status), each answering with
//! a typed result. The per-stack deploy slots live in one `std::sync::Mutex`, held only for
//! in-memory work, never across an `.await`.
//!
//! A deployment is driven by a spawned deploy task through the pure state machine of
//! [`machine`] (`(state, event) -> (state, effects)`); the effects are carried out by the
//! interpreter in `engine`. M3 has no proxy yet: health is the built-in "running for 5 s"
//! check, `Switching` commits the per-service pointers without publishing a `RouteTable`, and
//! `Draining` completes at once (architecture §5.1).

#![allow(
    dead_code,
    reason = "the orchestrator is wired into the supervisor and web UI by later tasks"
)]

mod engine;
pub mod machine;
mod plan;

use std::collections::BTreeMap;
use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::Notify;

use crate::ports::PortAllocator;
use crate::registry::{RegistryClient, RegistryError};
use crate::routing::{CollisionError, Reserved, RouteClaims};
use crate::runtime::{ContainerRuntime, NetworkSpec, ObjectKind, RuntimeError, VolumeMount, label};
use crate::secrets::{SecretError, SecretStore};
use crate::stack::{Diagnostic, ImageRef, Name, PinnedImage, Stack, StackError};
use crate::store::{
    Deployment, DeploymentId, DeploymentSource, DeploymentState, DigestSet, NewDeployment,
    StackRevision, Store, StoreError,
};

use engine::{Run, Target};
use machine::Event;
pub use plan::PlanError;
use plan::{Plan, SecretVersions};

/// Resolves a third-party image's tag to the digest the registry serves for it (ADR-0025).
/// [`RegistryClient`] in production; tests use a fake.
#[async_trait]
pub trait TagResolver: Send + Sync {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError>;
}

#[async_trait]
impl TagResolver for RegistryClient {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError> {
        RegistryClient::resolve(self, image).await
    }
}

/// Tunables of the orchestrator.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Hosts no stack may route (the admin host, ADR-0031).
    pub reserved: Reserved,
    /// The built-in health check: a new container must stay running this long (§5.1).
    pub settle: Duration,
    /// How often the built-in check inspects the container.
    pub settle_poll: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            reserved: Reserved::default(),
            settle: Duration::from_secs(5),
            settle_poll: Duration::from_millis(500),
        }
    }
}

/// Why an orchestrator call was refused.
#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("stack {0:?} does not exist")]
    UnknownStack(String),
    #[error("stack has no service {0:?}")]
    UnknownService(String),
    #[error("deployment {0} does not exist for this stack")]
    UnknownDeployment(DeploymentId),
    #[error(transparent)]
    Invalid(#[from] StackError),
    #[error(transparent)]
    Collision(#[from] CollisionError),
    #[error("cannot resolve the image of service {service}: {message}")]
    Resolve { service: String, message: String },
    #[error("service {0} does not use a CI-built image; its digest comes from the stack revision")]
    NotCiBuilt(String),
    #[error("invalid digest for service {service}: {message}")]
    Digest { service: String, message: String },
    #[error("deployment {id} cannot be redeployed: {reason}")]
    NotRedeployable { id: DeploymentId, reason: String },
    #[error("stack {0} has an active deployment")]
    StackBusy(String),
    #[error("stack {0} is being removed")]
    Removing(String),
    #[error("deleting the volumes of stack {stack} needs the stack's name typed as confirmation")]
    VolumeConfirmation { stack: String },
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<PlanError> for OrchestratorError {
    fn from(err: PlanError) -> Self {
        match err {
            PlanError::UnknownService(service) => Self::UnknownService(service),
            PlanError::Digest { service, message } => Self::Digest { service, message },
        }
    }
}

type Result<T, E = OrchestratorError> = std::result::Result<T, E>;

/// The reply to a deploy request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployOutcome {
    pub deployment: DeploymentId,
    /// `Noop` (nothing to do), `Queued` (another deployment is active) or the state the
    /// started deployment had when the call returned.
    pub state: DeploymentState,
}

/// The reply to [`Orchestrator::apply`].
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    /// The new active revision.
    pub revision: StackRevision,
    pub warnings: Vec<Diagnostic>,
    /// Services whose (digest, config hash) changes, or that leave the stack.
    pub changed: Vec<String>,
    /// The `source = apply` deployment, unless nothing changes (ADR-0049).
    pub deployment: Option<DeployOutcome>,
}

/// Why a deployment ended when its stack was removed under it.
const STACK_REMOVED: &str = "stack removed";

/// A request to remove a stack (ADR-0030).
///
/// Named volumes are kept unless `delete_volumes` is set, and deleting them needs an explicit
/// confirmation: the stack's name, typed again by the caller (the UI asks for it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveStack {
    pub stack: String,
    pub delete_volumes: bool,
    /// Must equal `stack` when `delete_volumes` is set.
    pub confirmation: Option<String>,
}

impl RemoveStack {
    /// Removes `stack` and keeps its volumes.
    pub fn keeping_volumes(stack: impl Into<String>) -> Self {
        Self {
            stack: stack.into(),
            delete_volumes: false,
            confirmation: None,
        }
    }

    /// Removes `stack` and deletes its volumes; `confirmation` must be the stack's name.
    pub fn deleting_volumes(stack: impl Into<String>, confirmation: impl Into<String>) -> Self {
        Self {
            stack: stack.into(),
            delete_volumes: true,
            confirmation: Some(confirmation.into()),
        }
    }

    /// Refuses a volume deletion without the right confirmation, before anything is touched.
    fn check(&self) -> Result<()> {
        if self.delete_volumes && self.confirmation.as_deref() != Some(self.stack.as_str()) {
            return Err(OrchestratorError::VolumeConfirmation {
                stack: self.stack.clone(),
            });
        }
        Ok(())
    }
}

/// The reply to [`Orchestrator::remove`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// How many containers were removed.
    pub containers: usize,
    /// The named volumes deleted; empty unless the request asked for it (ADR-0030).
    pub volumes: Vec<String>,
}

/// One stack as [`Orchestrator::status`] reports it.
#[derive(Debug, Clone)]
pub struct StackStatus {
    pub name: String,
    /// The active revision.
    pub revision: StackRevision,
    pub services: Vec<ServiceStatus>,
    /// The deployment running now, with its state.
    pub active: Option<(DeploymentId, DeploymentState)>,
    /// The deployment waiting for the active one (ADR-0040).
    pub queued: Option<DeploymentId>,
    /// The most recent deployment.
    pub last: Option<Deployment>,
    /// "revision N: deploy failed: …" when the latest deployment of the active revision failed
    /// (ADR-0049).
    pub flag: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub name: String,
    pub state: ServiceState,
}

/// What one service runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Running {
        generation: u32,
        digest: String,
    },
    /// A CI-built service that has never received a digest (no webhook yet).
    AwaitingFirstDigest,
    /// Has a digest but no deployment of it went live yet.
    NotDeployed,
}

/// The orchestrator. Construct once, share as `Arc<Orchestrator<R>>`.
pub struct Orchestrator<R> {
    store: Store,
    runtime: Arc<R>,
    resolver: Arc<dyn TagResolver>,
    ports: PortAllocator,
    /// App secrets, injected at container create (ADR-0029).
    secrets: SecretStore,
    settings: Settings,
    slots: Mutex<BTreeMap<String, Slot>>,
    /// Signalled whenever a deployment leaves its stack's slot.
    finished: Notify,
    /// Signalled when a stack's removal starts, so its deploy task aborts.
    removal: Notify,
}

/// The deploy slot of one stack (ADR-0040): one active deployment, one pending.
#[derive(Default)]
struct Slot {
    active: Option<(DeploymentId, DeploymentState)>,
    pending: Option<Run>,
    /// The stack is being removed: the active deployment aborts and nothing new is accepted.
    removing: bool,
}

/// Clears a stack's `removing` mark when [`Orchestrator::remove`] returns, also on an error. A
/// finished removal has dropped the slot already.
struct Removal<'a, R> {
    orchestrator: &'a Orchestrator<R>,
    stack: &'a str,
}

impl<R> Drop for Removal<'_, R> {
    fn drop(&mut self) {
        let mut slots = self
            .orchestrator
            .slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = slots.get_mut(self.stack) {
            slot.removing = false;
        }
    }
}

/// Whether a submitted deployment got the stack's slot.
enum Claim {
    Free,
    Busy,
    /// The stack is being removed.
    Removing,
}

/// Where [`Orchestrator::enqueue`] put a queued deployment.
enum Enqueued {
    /// The slot was free after all; the run is now active and handed back to be driven.
    Active(Run),
    /// The run is the stack's pending deployment; carries the pending one it replaced.
    Pending(Option<Run>),
    /// The stack is being removed; the run is handed back to be ended.
    Removing(Run),
}

impl<R: ContainerRuntime> Orchestrator<R> {
    pub fn new(
        store: Store,
        runtime: Arc<R>,
        resolver: Arc<dyn TagResolver>,
        ports: PortAllocator,
        secrets: SecretStore,
        settings: Settings,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            runtime,
            resolver,
            ports,
            secrets,
            settings,
            slots: Mutex::default(),
            finished: Notify::new(),
            removal: Notify::new(),
        })
    }

    /// Applies a stack file (ADR-0027, ADR-0049): validates it, checks its routes against the
    /// other stacks, resolves third-party tags to digests (ADR-0025), stores it as the active
    /// revision and redeploys every service whose (digest, config hash) changes.
    #[tracing::instrument(skip_all)]
    pub async fn apply(self: &Arc<Self>, toml: &str) -> Result<ApplyOutcome> {
        let parsed = Stack::parse(toml)?;
        let definition = Arc::new(parsed.stack);
        if self.is_removing(definition.name.as_ref()) {
            return Err(OrchestratorError::Removing(definition.name.to_string()));
        }
        self.check_routes(&definition).await?;
        let images = self.resolve_images(&definition).await?;
        let name = definition.name.to_string();
        let revision = self.store.add_pinned_revision(&name, toml, &images).await?;
        tracing::info!(stack = %name, revision = revision.revision, "applied stack revision");
        let running = self.store.services(&name).await?;
        let secrets = self.secret_versions(&definition).await?;
        let plan = Plan::new(&definition, &images, &DigestSet::new(), &running, &secrets)?;
        let changed = plan
            .changes
            .iter()
            .map(|c| c.service.to_string())
            .chain(plan.removed.iter().map(|p| p.service.clone()))
            .collect();
        let deployment = if plan.is_noop() && !self.is_busy(&name) {
            None
        } else {
            let target = Target::new(definition, revision.clone(), images, DigestSet::new());
            Some(self.submit(target, DeploymentSource::Apply).await?)
        };
        Ok(ApplyOutcome {
            revision,
            warnings: parsed.warnings,
            changed,
            deployment,
        })
    }

    /// Deploys `images` (service → digest, CI-built services only) to the stack's active
    /// revision. Services left out keep their digest. A target that already runs is a `Noop`.
    #[tracing::instrument(skip(self, images))]
    pub async fn deploy(
        self: &Arc<Self>,
        stack: &str,
        images: DigestSet,
        source: DeploymentSource,
    ) -> Result<DeployOutcome> {
        let revision = self
            .store
            .latest_revision(stack)
            .await?
            .ok_or_else(|| OrchestratorError::UnknownStack(stack.to_owned()))?;
        let definition: Arc<Stack> = Arc::new(revision.toml.parse()?);
        for (service, digest) in &images {
            let image = definition
                .services
                .iter()
                .find(|(n, _)| n.as_ref() == service)
                .map(|(_, s)| &s.image)
                .ok_or_else(|| OrchestratorError::UnknownService(service.clone()))?;
            if !image.is_ci_built() {
                return Err(OrchestratorError::NotCiBuilt(service.clone()));
            }
            digest.parse::<crate::stack::Digest>().map_err(|message| {
                OrchestratorError::Digest {
                    service: service.clone(),
                    message,
                }
            })?;
        }
        let pins = self.store.revision_images(revision.id).await?;
        self.submit(Target::new(definition, revision, pins, images), source)
            .await
    }

    /// Redeploys an earlier successful deployment: its digest set and its stack revision
    /// (ADR-0041). An older revision becomes the active one again as a new revision.
    #[tracing::instrument(skip(self))]
    pub async fn rollback(
        self: &Arc<Self>,
        stack: &str,
        deployment: DeploymentId,
    ) -> Result<DeployOutcome> {
        // A removed stack keeps its history, but rolling back must not revive it.
        if self.store.stack(stack).await?.is_none() {
            return Err(OrchestratorError::UnknownStack(stack.to_owned()));
        }
        let earlier = self
            .store
            .deployment(deployment)
            .await?
            .filter(|d| d.stack == stack)
            .ok_or(OrchestratorError::UnknownDeployment(deployment))?;
        let refuse = |reason: &str| OrchestratorError::NotRedeployable {
            id: deployment,
            reason: reason.to_owned(),
        };
        if !matches!(
            earlier.state,
            DeploymentState::Live | DeploymentState::LiveWithWarnings
        ) {
            return Err(refuse("it never went live"));
        }
        let old = match earlier.revision {
            Some(id) => self.store.revision(id).await?,
            None => None,
        }
        .ok_or_else(|| refuse("its stack revision is gone"))?;
        let pins = self.store.revision_images(old.id).await?;
        let latest = self.store.latest_revision(stack).await?;
        let revision = match latest {
            Some(latest) if latest.id == old.id => latest,
            _ => {
                // The old revision becomes the active one again, so it must pass the same
                // checks as an apply: another stack may have claimed its routes since.
                self.check_routes(&old.toml.parse::<Stack>()?).await?;
                self.store
                    .add_pinned_revision(stack, &old.toml, &pins)
                    .await?
            }
        };
        let definition: Arc<Stack> = Arc::new(revision.toml.parse()?);
        let target = Target::new(definition, revision, pins, earlier.target);
        self.submit(target, DeploymentSource::Rollback).await
    }

    /// Removes a stack (ADR-0030): aborts its active deployment and drops its queued one, then
    /// removes its containers, its network, its service pointers and port reservations. The
    /// revisions and deployment history are kept. Named volumes are kept too, unless the
    /// request asks to delete them and carries the stack's name as confirmation.
    ///
    /// Deleting the volumes is also the separate action ADR-0030 describes: asked for a stack
    /// that was removed earlier with its volumes kept, it deletes just those volumes.
    #[tracing::instrument(skip(self), fields(stack = %request.stack))]
    pub async fn remove(self: &Arc<Self>, request: RemoveStack) -> Result<RemoveOutcome> {
        request.check()?;
        let stack = request.stack.as_str();
        let unknown = || OrchestratorError::UnknownStack(stack.to_owned());
        let name: Name = stack.parse().map_err(|_| unknown())?;
        if self.store.stack(stack).await?.is_none() {
            if request.delete_volumes && !self.store.revisions(stack).await?.is_empty() {
                return self.remove_leftover_volumes(&name).await;
            }
            return Err(unknown());
        }
        let (active, pending) = {
            let mut slots = self.lock();
            let slot = slots.entry(stack.to_owned()).or_default();
            if slot.removing {
                return Err(OrchestratorError::Removing(stack.to_owned()));
            }
            slot.removing = true;
            (slot.active.map(|(id, _)| id), slot.pending.take())
        };
        let _removing = Removal {
            orchestrator: self,
            stack,
        };
        // Wake a deploy task waiting in a health check or a pull, so it aborts now.
        self.removal.notify_waiters();
        if let Some(pending) = pending {
            self.store
                .transition(
                    pending.id(),
                    DeploymentState::Superseded,
                    Some(STACK_REMOVED),
                )
                .await?;
            self.finished.notify_waiters();
        }
        if let Some(active) = active {
            tracing::info!(stack, deployment = %active, "aborting the active deployment");
            let ended = self.wait(active).await?;
            tracing::info!(stack, deployment = %active, state = %ended.state, "deployment ended");
        }
        // Every container labelled with the stack, including leftovers of interrupted deploys.
        let containers: Vec<_> = self
            .runtime
            .list_managed()
            .await?
            .into_iter()
            .filter(|c| c.labels.get(label::STACK).is_some_and(|s| s == stack))
            .collect();
        for container in &containers {
            self.runtime.remove(&container.id).await?;
        }
        match self
            .runtime
            .remove_network(&NetworkSpec::for_stack(&name).name)
            .await
        {
            Err(RuntimeError::NotFound {
                kind: ObjectKind::Network,
                ..
            })
            | Ok(()) => {}
            Err(err) => return Err(err.into()),
        }
        let volumes = if request.delete_volumes {
            self.remove_volumes(&name).await?
        } else {
            Vec::new()
        };
        self.store.remove_stack(stack).await?;
        self.lock().remove(stack);
        tracing::info!(
            stack,
            containers = containers.len(),
            volumes = volumes.len(),
            "removed stack"
        );
        Ok(RemoveOutcome {
            containers: containers.len(),
            volumes,
        })
    }

    /// Deletes the kept volumes of a stack that is removed already. The stack's slot is marked
    /// as being removed meanwhile, so an apply cannot start a deployment that mounts them.
    async fn remove_leftover_volumes(&self, stack: &Name) -> Result<RemoveOutcome> {
        {
            let mut slots = self.lock();
            let slot = slots.entry(stack.to_string()).or_default();
            if slot.removing {
                return Err(OrchestratorError::Removing(stack.to_string()));
            }
            if slot.active.is_some() {
                // Applied again since the check: the stack is live, its volumes are in use.
                return Err(OrchestratorError::StackBusy(stack.to_string()));
            }
            slot.removing = true;
        }
        let _removing = Removal {
            orchestrator: self,
            stack: stack.as_ref(),
        };
        let volumes = self.remove_volumes(stack).await?;
        tracing::info!(%stack, volumes = volumes.len(), "deleted the volumes of a removed stack");
        Ok(RemoveOutcome {
            containers: 0,
            volumes,
        })
    }

    /// Deletes every named volume any revision of the stack declared (`hoster-<stack>-<volume>`,
    /// ADR-0030). A volume that was never created is skipped. Returns the deleted names.
    async fn remove_volumes(&self, stack: &Name) -> Result<Vec<String>> {
        let mut names = std::collections::BTreeSet::new();
        for revision in self.store.revisions(stack.as_ref()).await? {
            let definition = match revision.toml.parse::<Stack>() {
                Ok(definition) => definition,
                Err(err) => {
                    tracing::warn!(%stack, revision = revision.revision, %err, "stored revision no longer parses; its volumes are not deleted");
                    continue;
                }
            };
            for service in definition.services.values() {
                for (volume, target) in &service.volumes {
                    if let Ok(mount) = VolumeMount::named(stack, volume, target) {
                        names.insert(mount.volume().to_owned());
                    }
                }
            }
        }
        let mut removed = Vec::new();
        for volume in names {
            match self.runtime.remove_volume(&volume).await {
                Ok(()) => {
                    tracing::info!(%stack, volume, "deleted volume");
                    removed.push(volume);
                }
                Err(RuntimeError::NotFound {
                    kind: ObjectKind::Volume,
                    ..
                }) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(removed)
    }

    /// Every stack with its active revision, services and deployments.
    pub async fn status(&self) -> Result<Vec<StackStatus>> {
        let mut out = Vec::new();
        for stack in self.store.stacks().await? {
            let Some(revision) = self.store.latest_revision(&stack.name).await? else {
                continue;
            };
            let pointers = self.store.services(&stack.name).await?;
            let pins = self.store.revision_images(revision.id).await?;
            let services = match revision.toml.parse::<Stack>() {
                Ok(definition) => definition
                    .services
                    .iter()
                    .map(|(name, service)| {
                        let state = match pointers.iter().find(|p| p.service == name.as_ref()) {
                            Some(p) => ServiceState::Running {
                                generation: p.generation,
                                digest: p.digest.clone(),
                            },
                            None if service.image.is_ci_built()
                                && !pins.contains_key(name.as_ref()) =>
                            {
                                ServiceState::AwaitingFirstDigest
                            }
                            None => ServiceState::NotDeployed,
                        };
                        ServiceStatus {
                            name: name.to_string(),
                            state,
                        }
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            let last = self.store.deployments(&stack.name, 1).await?.pop();
            let flag = last
                .as_ref()
                .filter(|d| {
                    matches!(d.state, DeploymentState::Failed | DeploymentState::Rejected)
                        && d.revision == Some(revision.id)
                })
                .map(|d| {
                    format!(
                        "revision {}: deploy failed: {}",
                        revision.revision,
                        d.error.as_deref().unwrap_or("unknown error")
                    )
                });
            let (active, queued) = {
                let slots = self.lock();
                let slot = slots.get(&stack.name);
                (
                    slot.and_then(|s| s.active),
                    slot.and_then(|s| s.pending.as_ref().map(Run::id)),
                )
            };
            out.push(StackStatus {
                name: stack.name,
                revision,
                services,
                active,
                queued,
                last,
                flag,
            });
        }
        Ok(out)
    }

    /// Waits until `id` is over (a terminal state, or halted) and returns it.
    pub async fn wait(&self, id: DeploymentId) -> Result<Deployment> {
        loop {
            let mut notified = pin!(self.finished.notified());
            notified.as_mut().enable();
            let deployment = self
                .store
                .deployment(id)
                .await?
                .ok_or(OrchestratorError::UnknownDeployment(id))?;
            if deployment.state.is_terminal() || !self.holds(&deployment.stack, id) {
                return Ok(deployment);
            }
            notified.await;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Deploy slots
    // -----------------------------------------------------------------------------------------

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Slot>> {
        // Slots hold plain data; a panic cannot leave them half-updated.
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn is_busy(&self, stack: &str) -> bool {
        self.lock().get(stack).is_some_and(|s| s.active.is_some())
    }

    /// Whether the stack's removal has started.
    fn is_removing(&self, stack: &str) -> bool {
        self.lock().get(stack).is_some_and(|s| s.removing)
    }

    /// Whether `id` is the active or pending deployment of `stack`.
    fn holds(&self, stack: &str, id: DeploymentId) -> bool {
        self.lock().get(stack).is_some_and(|s| {
            s.active.is_some_and(|(a, _)| a == id)
                || s.pending.as_ref().is_some_and(|p| p.id() == id)
        })
    }

    /// Takes the stack's slot for `id` if it is free.
    fn claim(&self, stack: &str, id: DeploymentId, state: DeploymentState) -> Claim {
        let mut slots = self.lock();
        let slot = slots.entry(stack.to_owned()).or_default();
        if slot.removing {
            Claim::Removing
        } else if slot.active.is_some() {
            Claim::Busy
        } else {
            slot.active = Some((id, state));
            Claim::Free
        }
    }

    /// Under one lock: takes the stack's slot for `run` if it is free by now, or makes `run` the
    /// pending deployment. Both must happen in the same critical section, or the active
    /// deployment could end between the check and the insert and leave `run` pending forever
    /// behind nothing.
    fn enqueue(&self, stack: &str, run: Run) -> Enqueued {
        let mut slots = self.lock();
        let slot = slots.entry(stack.to_owned()).or_default();
        if slot.removing {
            Enqueued::Removing(run)
        } else if slot.active.is_none() {
            slot.active = Some((run.id(), run.state()));
            Enqueued::Active(run)
        } else {
            Enqueued::Pending(slot.pending.replace(run))
        }
    }

    /// Records a new deployment, then runs its idempotency check right away if the stack is
    /// free, or queues it behind the active one (ADR-0040).
    async fn submit(
        self: &Arc<Self>,
        target: Target,
        source: DeploymentSource,
    ) -> Result<DeployOutcome> {
        let stack = target.stack().to_string();
        if self.is_removing(&stack) {
            return Err(OrchestratorError::Removing(stack));
        }
        let running = self.store.services(&stack).await?;
        let secrets = self.secret_versions(target.definition()).await?;
        let digests = target.plan(&running, &secrets)?.target();
        let record = self
            .store
            .create_deployment(&NewDeployment {
                stack: stack.clone(),
                revision: Some(target.revision_id()),
                source,
                target: digests,
            })
            .await?;
        let id = record.id;
        let mut run = Run::new(id, target, record.target);
        let first = match self.claim(&stack, id, DeploymentState::Received) {
            Claim::Free => self.check_running(&mut run).await,
            Claim::Removing => {
                // The removal started after the check above.
                self.store
                    .transition(id, DeploymentState::Rejected, Some(STACK_REMOVED))
                    .await?;
                return Err(OrchestratorError::Removing(stack));
            }
            Claim::Busy => {
                // Persist `Queued` before the run can be dequeued, then look again: the active
                // deployment may have ended meanwhile.
                self.step(&mut run, Event::Busy).await;
                match self.enqueue(&stack, run) {
                    Enqueued::Active(active) => {
                        run = active;
                        Event::Dequeued
                    }
                    Enqueued::Removing(_) => {
                        self.store
                            .transition(id, DeploymentState::Superseded, Some(STACK_REMOVED))
                            .await?;
                        self.finished.notify_waiters();
                        return Err(OrchestratorError::Removing(stack));
                    }
                    Enqueued::Pending(superseded) => {
                        if let Some(mut old) = superseded {
                            self.step(&mut old, Event::Superseded).await;
                            self.finished.notify_waiters();
                        }
                        return Ok(DeployOutcome {
                            deployment: id,
                            state: DeploymentState::Queued,
                        });
                    }
                }
            }
        };
        if first == Event::Unchanged {
            // Finish inline so the caller gets the `Noop` answer.
            let this = Arc::clone(self);
            this.drive(run, first).await;
            return Ok(DeployOutcome {
                deployment: id,
                state: DeploymentState::Noop,
            });
        }
        let state = run.state();
        tokio::spawn(Arc::clone(self).drive(run, first));
        Ok(DeployOutcome {
            deployment: id,
            state,
        })
    }

    /// Frees the stack's slot after `id` and starts the pending deployment, if any.
    fn release(self: &Arc<Self>, stack: &str, id: DeploymentId) {
        let next = {
            let mut slots = self.lock();
            let slot = slots.entry(stack.to_owned()).or_default();
            if slot.active.is_some_and(|(a, _)| a == id) {
                slot.active = None;
                let next = slot.pending.take();
                if let Some(next) = &next {
                    slot.active = Some((next.id(), DeploymentState::Queued));
                }
                next
            } else {
                None
            }
        };
        self.finished.notify_waiters();
        if let Some(next) = next {
            tokio::spawn(Arc::clone(self).drive(next, Event::Dequeued));
        }
    }

    // -----------------------------------------------------------------------------------------
    // Apply helpers
    // -----------------------------------------------------------------------------------------

    /// Checks `candidate`'s routes against every other stack's active revision (ADR-0012).
    async fn check_routes(&self, candidate: &Stack) -> Result<()> {
        let others = self.other_stacks(&candidate.name).await?;
        RouteClaims::check(candidate, &others, &self.settings.reserved)?;
        Ok(())
    }

    /// The active definitions of every stack except `name`.
    async fn other_stacks(&self, name: &Name) -> Result<Vec<Stack>> {
        let mut others = Vec::new();
        for stack in self.store.stacks().await? {
            if stack.name == name.as_ref() {
                continue;
            }
            if let Some(revision) = self.store.latest_revision(&stack.name).await? {
                match revision.toml.parse::<Stack>() {
                    Ok(other) => others.push(other),
                    Err(err) => {
                        tracing::warn!(stack = %stack.name, %err, "stored revision no longer parses")
                    }
                }
            }
        }
        Ok(others)
    }

    /// The current versions of every secret `stack` references; unset ones are left out.
    async fn secret_versions(&self, stack: &Stack) -> Result<SecretVersions> {
        let mut versions = SecretVersions::new();
        for name in stack.services.values().flat_map(|s| s.secrets.values()) {
            if versions.contains_key(name) {
                continue;
            }
            if let Some(version) = self.secrets.version(name).await? {
                versions.insert(name.clone(), version);
            }
        }
        Ok(versions)
    }

    /// Pins every third-party image: a digest in the file is kept, a tag is resolved
    /// (ADR-0025). CI-built images get their digest from the deploy.
    async fn resolve_images(&self, stack: &Stack) -> Result<DigestSet> {
        let mut pins = DigestSet::new();
        for (name, service) in &stack.services {
            if service.image.is_ci_built() {
                continue;
            }
            let digest = match service.image.digest() {
                Some(digest) => digest.clone(),
                None => self
                    .resolver
                    .resolve(&service.image)
                    .await
                    .map_err(|err| OrchestratorError::Resolve {
                        service: name.to_string(),
                        message: err.to_string(),
                    })?
                    .digest()
                    .clone(),
            };
            pins.insert(name.to_string(), digest.to_string());
        }
        Ok(pins)
    }
}

#[cfg(test)]
mod tests;
