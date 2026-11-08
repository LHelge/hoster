//! The `Orchestrator` handle (ADR-0050): deploy state machine, per-stack deploy tasks and the
//! reconciler that keeps containers running (ADR-0028).
//!
//! [`Orchestrator`] is the single writer of desired state. Callers (the web UI, the webhook, the
//! poller) hold an `Arc<Orchestrator>` and call its async methods:
//! [`preview`](Orchestrator::preview) and [`apply`](Orchestrator::apply),
//! [`deploy`](Orchestrator::deploy) and [`deploy_webhook`](Orchestrator::deploy_webhook),
//! [`remove`](Orchestrator::remove) (see [`RemoveStack`]),
//! [`rollback`](Orchestrator::rollback), [`status`](Orchestrator::status) and
//! [`deployment`](Orchestrator::deployment), each answering with a typed result. The per-stack deploy slots live in one `std::sync::Mutex`, held only for
//! in-memory work, never across an `.await`.
//!
//! A deployment is driven by a spawned deploy task through the pure state machine of
//! [`machine`] (`(state, event) -> (state, effects)`); the effects are carried out by the
//! interpreter in `engine`: new containers start next to the old ones on fresh ports, are probed
//! with their resolved health checks (ADR-0038), the whole stack swaps in one `RouteTable`
//! publish plus one pointer transaction, and the old upstreams drain on their in-flight
//! counters before they are removed (ADR-0017, ADR-0048, architecture §5).
//!
//! The orchestrator is also the single writer of the proxy's `ArcSwap<RouteTable>` (`routes`,
//! architecture §6.4): desired state lives under the same lock as the slots, and every change
//! (a deployment's commit, a stop-first gap, a removal, the startup restore) rebuilds and
//! publishes the whole table inside that critical section.
//!
//! The reconciler (`reconcile`, ADR-0028) resolves interrupted deployments and adopts, starts
//! or removes containers at startup ([`Orchestrator::recover`]), then keeps the current
//! generation running with backoff ([`Orchestrator::run_reconciler`]).
//!
//! The health monitor (`monitor`, ADR-0038) keeps probing every live generation
//! ([`Orchestrator::run_health_monitor`]) and reports status changes on `ui_events`
//! ([`Orchestrator::subscribe`]) and in [`Orchestrator::status`].
//!
//! Certificates (`certs`, ADR-0036): every apply tells the ACME task which hosts to ensure and
//! which to forget ([`Orchestrator::connect_acme`]); the task's events are recorded and relayed
//! on `ui_events` ([`Orchestrator::run_acme_events`], [`Orchestrator::certificates`]).

#![allow(
    dead_code,
    reason = "the orchestrator is wired into the supervisor and web UI by later tasks"
)]

mod certs;
mod engine;
pub mod machine;
mod monitor;
mod plan;
mod reconcile;
mod routes;
mod stack_secrets;

use std::collections::BTreeMap;
use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{Notify, broadcast};

use crate::health::{HealthStats, HealthStatus, MonitorSettings, NetworkProber, Prober};
use crate::ports::{PortAllocator, PortError};
use crate::registry::{RegistryClient, RegistryError};
use crate::routing::{CollisionError, Reserved, RouteClaims, RouteTable};
use crate::runtime::{ContainerRuntime, NetworkSpec, ObjectKind, RuntimeError, VolumeMount, label};
use crate::secrets::{SecretError, SecretStore};
use crate::stack::{
    DeployCaller, Diagnostic, Digest, ImageRef, Name, PinnedImage, Stack, StackError,
};
use crate::store::{
    Deployment, DeploymentEvent, DeploymentId, DeploymentSource, DeploymentState, DigestSet,
    NewDeployment, Provenance, StackRevision, Store, StoreError, WebhookRejection,
};

use certs::CertHosts;
#[allow(unused_imports, reason = "read by the certificates page of the web UI")]
pub use certs::CertStatus;
use engine::{Run, Target};
use machine::Event;
pub use plan::PlanError;
use plan::{Plan, SecretVersions};
use reconcile::Reconciler;
pub use reconcile::{RestartPolicy, RestartStats};
use routes::Routes;
pub use stack_secrets::{SecretChange, StackSecret};

/// The registry lookups the orchestrator needs: a third-party image's tag resolved to the
/// digest the registry serves for it (ADR-0025), and whether a webhook's digest is in its
/// service's repository (ADR-0042). [`RegistryClient`] in production; tests use a fake.
#[async_trait]
pub trait TagResolver: Send + Sync {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError>;

    /// `Ok` when `image`'s repository holds `digest`; [`RegistryError::NotFound`] when not.
    async fn contains(&self, image: &ImageRef, digest: &Digest) -> Result<(), RegistryError>;
}

#[async_trait]
impl TagResolver for RegistryClient {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError> {
        RegistryClient::resolve(self, image).await
    }

    async fn contains(&self, image: &ImageRef, digest: &Digest) -> Result<(), RegistryError> {
        RegistryClient::contains(self, image, digest).await
    }
}

/// Tunables of the orchestrator.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Hosts no stack may route (the admin host, ADR-0031).
    pub reserved: Reserved,
    /// The health check of a service without routes and without a declared `health` table: a
    /// new container must stay running this long (ADR-0038).
    pub settle: Duration,
    /// How often that check inspects the container.
    pub settle_poll: Duration,
    /// The wait after the swap before the old upstreams' in-flight counters are watched, so a
    /// request that loaded the old `RouteTable` just before is counted (ADR-0035 #2, §5.3).
    pub drain_settle: Duration,
    /// How the reconciler restarts crashed containers (ADR-0028).
    pub restart: RestartPolicy,
    /// How often live services are probed (ADR-0038).
    pub monitor: MonitorSettings,
    /// The `Retry-After` of a stop-first service's routes during its gap (ADR-0048).
    pub gap_retry_after: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            reserved: Reserved::default(),
            settle: Duration::from_secs(5),
            settle_poll: Duration::from_millis(500),
            drain_settle: Duration::from_secs(1),
            restart: RestartPolicy::default(),
            monitor: MonitorSettings::default(),
            gap_retry_after: Duration::from_secs(5),
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
    /// A webhook may not deploy this stack (ADR-0047): it does not exist, has no `[deploy]`
    /// rule, or the caller does not match it. `reason` is for the admin only.
    #[error("not allowed to deploy stack {stack}: {reason}")]
    Forbidden { stack: String, reason: String },
    #[error("the digest for service {service} is not in its repository {repository}")]
    ForeignDigest { service: String, repository: String },
    #[error("deployment {id} cannot be redeployed: {reason}")]
    NotRedeployable { id: DeploymentId, reason: String },
    #[error("stack {0} has an active deployment")]
    StackBusy(String),
    #[error("stack {0} is being removed")]
    Removing(String),
    #[error(
        "stack {stack} is at revision {active} now, not revision {expected}; review the change again"
    )]
    RevisionChanged {
        stack: String,
        expected: u32,
        active: u32,
    },
    #[error("deleting the volumes of stack {stack} needs the stack's name typed as confirmation")]
    VolumeConfirmation { stack: String },
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Secret(#[from] SecretError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Ports(#[from] PortError),
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

/// A deploy request from the webhook (ADR-0042, ADR-0047), with its verified caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookDeploy {
    pub stack: String,
    /// Service → digest; CI-built services only, each digest from the service's repository.
    pub images: DigestSet,
    /// Checked against the stack's `[deploy]` rule.
    pub caller: DeployCaller,
    /// Recorded on the deployment.
    pub provenance: Provenance,
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

/// The reply to [`Orchestrator::preview`]: what applying a stack file would do.
#[derive(Debug, Clone)]
pub struct ApplyPreview {
    /// The stack the file defines.
    pub stack: String,
    /// The stack's active revision, which the file would replace; `None` for a new stack.
    pub current: Option<StackRevision>,
    pub warnings: Vec<Diagnostic>,
    /// The digest every third-party image resolves to, next to the active revision's.
    pub pins: Vec<PinChange>,
    /// Services the apply would redeploy: their (digest, config hash) changes, or they leave
    /// the stack.
    pub changed: Vec<String>,
}

/// A third-party service's pinned digest (ADR-0025), before and after an apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinChange {
    pub service: String,
    /// The active revision's digest; `None` when the service is new.
    pub before: Option<String>,
    pub after: String,
}

/// A stack file that passed the apply checks.
struct Prepared {
    definition: Arc<Stack>,
    images: DigestSet,
    warnings: Vec<Diagnostic>,
    changed: Vec<String>,
    noop: bool,
}

/// What open UI pages are told about (architecture §3.3 #5). A receiver that lags just
/// re-renders from [`Orchestrator::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiEvent {
    /// A live service's health status changed (ADR-0038).
    Health {
        stack: String,
        service: String,
        generation: u32,
        from: HealthStatus,
        to: HealthStatus,
        /// The probe result (or reconciler verdict) behind the change.
        detail: String,
    },
    /// The ACME task issued a certificate, failed to, or found a host's DNS not pointing here
    /// (architecture §3.3 #2).
    Certificate(crate::acme::AcmeEvent),
}

/// How many `ui_events` a slow receiver may fall behind before it lags.
const UI_EVENTS_CAPACITY: usize = 256;

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
    /// The latest refused webhook attempts, newest first (ADR-0047).
    pub rejections: Vec<WebhookRejection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub name: String,
    /// The image reference as the stack file writes it.
    pub image: String,
    /// Whether the image is built in CI, so its digest comes from deploys (webhook, poller or
    /// a manual deploy) rather than from the stack revision.
    pub ci_built: bool,
    pub state: ServiceState,
    /// How often the reconciler restarted the running generation, and whether it is crash
    /// looping (ADR-0028).
    pub restarts: RestartStats,
    /// The running generation's liveness and probe statistics (ADR-0038, ADR-0045).
    pub health: HealthStats,
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
    /// Desired state and the deploy slots, behind the one lock (ADR-0050).
    state: Mutex<State>,
    /// Signalled whenever a deployment leaves its stack's slot.
    finished: Notify,
    /// Signalled when a stack's removal starts, so its deploy task aborts.
    removal: Notify,
    /// Restart histories and warnings of the reconciler (ADR-0028).
    reconciler: Mutex<Reconciler>,
    /// Liveness of the live generations (ADR-0038).
    watchlist: Mutex<monitor::Watchlist>,
    /// Carries out the health probes, of deployments and of the health monitor.
    prober: Arc<dyn Prober>,
    /// Status changes for open UI pages (architecture §3.3 #5).
    ui_events: broadcast::Sender<UiEvent>,
}

/// What the orchestrator's lock guards: only in-memory data, never held across an `.await`.
struct State {
    /// The deploy slot of each stack.
    slots: BTreeMap<String, Slot>,
    /// Desired state and the `RouteTable` published from it (architecture §6.4).
    routes: Routes,
    /// The routed hosts the ACME task is told about, and their certificate status.
    certs: CertHosts,
}

/// The deploy slot of one stack (ADR-0040): one active deployment, one pending.
#[derive(Default)]
struct Slot {
    active: Option<(DeploymentId, DeploymentState)>,
    pending: Option<Run>,
    /// The stack is being removed: the active deployment aborts and nothing new is accepted.
    removing: bool,
    /// A reconcile pass is working on the stack's containers (ADR-0028). It only takes a slot
    /// with no active deployment, and a deployment submitted meanwhile waits as the pending one,
    /// so the two never touch the same containers at once.
    reconciling: bool,
}

impl Slot {
    /// Makes the pending deployment the active one, if the slot is free for it.
    fn promote_pending(&mut self) -> Option<Run> {
        if self.active.is_some() || self.reconciling || self.removing {
            return None;
        }
        let next = self.pending.take()?;
        self.active = Some((next.id(), DeploymentState::Queued));
        Some(next)
    }
}

/// A stack's slot held by a reconcile pass ([`Orchestrator::reconcile_claim`]). Dropping it
/// frees the slot and starts the deployment that was submitted meanwhile, if any.
pub(super) struct ReconcileClaim<'a, R: ContainerRuntime> {
    orchestrator: &'a Arc<Orchestrator<R>>,
    stack: String,
}

impl<R: ContainerRuntime> Drop for ReconcileClaim<'_, R> {
    fn drop(&mut self) {
        let next = {
            let mut state = self.orchestrator.lock();
            state.slots.get_mut(&self.stack).and_then(|slot| {
                slot.reconciling = false;
                slot.promote_pending()
            })
        };
        self.orchestrator.finished.notify_waiters();
        if let Some(next) = next {
            tokio::spawn(Arc::clone(self.orchestrator).drive(next, Event::Dequeued));
        }
    }
}

/// Clears a stack's `removing` mark when [`Orchestrator::remove`] returns, also on an error. A
/// finished removal has dropped the slot already.
struct Removal<'a, R: ContainerRuntime> {
    orchestrator: &'a Orchestrator<R>,
    stack: &'a str,
}

impl<R: ContainerRuntime> Drop for Removal<'_, R> {
    fn drop(&mut self) {
        let mut state = self.orchestrator.lock();
        if let Some(slot) = state.slots.get_mut(self.stack) {
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
    /// How many refused webhook attempts [`status`](Self::status) reports per stack.
    const SHOWN_REJECTIONS: u32 = 10;

    pub fn new(
        store: Store,
        runtime: Arc<R>,
        resolver: Arc<dyn TagResolver>,
        ports: PortAllocator,
        secrets: SecretStore,
        settings: Settings,
        routes: Arc<ArcSwap<RouteTable>>,
    ) -> Arc<Self> {
        Self::with_prober(
            store,
            runtime,
            resolver,
            ports,
            secrets,
            settings,
            routes,
            Arc::new(NetworkProber),
        )
    }

    /// [`Self::new`] with deployments and the health monitor probing through `prober`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the orchestrator's collaborators, each injected for tests"
    )]
    pub fn with_prober(
        store: Store,
        runtime: Arc<R>,
        resolver: Arc<dyn TagResolver>,
        ports: PortAllocator,
        secrets: SecretStore,
        settings: Settings,
        routes: Arc<ArcSwap<RouteTable>>,
        prober: Arc<dyn Prober>,
    ) -> Arc<Self> {
        let routes = Routes::new(routes, settings.reserved.clone(), settings.gap_retry_after);
        Arc::new(Self {
            store,
            runtime,
            resolver,
            ports,
            secrets,
            settings,
            state: Mutex::new(State {
                slots: BTreeMap::new(),
                routes,
                certs: CertHosts::default(),
            }),
            finished: Notify::new(),
            removal: Notify::new(),
            reconciler: Mutex::default(),
            watchlist: Mutex::default(),
            prober,
            ui_events: broadcast::channel(UI_EVENTS_CAPACITY).0,
        })
    }

    /// Checks a stack file the way [`apply`](Self::apply) would, without storing or deploying
    /// anything (ADR-0027, ADR-0049): validates it, checks its routes, resolves third-party
    /// tags (ADR-0025) and lists the services an apply would redeploy. The UI shows this as the
    /// diff the admin confirms. The apply itself resolves the tags again.
    #[tracing::instrument(skip_all)]
    pub async fn preview(&self, toml: &str) -> Result<ApplyPreview> {
        let prepared = self.prepare(toml).await?;
        let stack = prepared.definition.name.to_string();
        let current = self.store.latest_revision(&stack).await?;
        let before = match &current {
            Some(revision) => self.store.revision_images(revision.id).await?,
            None => DigestSet::new(),
        };
        let pins = prepared
            .images
            .iter()
            .map(|(service, digest)| PinChange {
                service: service.clone(),
                before: before.get(service).cloned(),
                after: digest.clone(),
            })
            .collect();
        Ok(ApplyPreview {
            stack,
            current,
            warnings: prepared.warnings,
            pins,
            changed: prepared.changed,
        })
    }

    /// Receives the [`UiEvent`]s sent from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.ui_events.subscribe()
    }

    /// Sends `event` to the open UI pages, if there are any.
    fn emit(&self, event: UiEvent) {
        // No receiver is not an error: no UI page is open.
        let _ = self.ui_events.send(event);
    }

    /// Applies a stack file (ADR-0027, ADR-0049): validates it, checks its routes against the
    /// other stacks, resolves third-party tags to digests (ADR-0025), stores it as the active
    /// revision and redeploys every service whose (digest, config hash) changes.
    #[tracing::instrument(skip_all)]
    pub async fn apply(self: &Arc<Self>, toml: &str) -> Result<ApplyOutcome> {
        let prepared = self.prepare(toml).await?;
        self.store_and_redeploy(toml, prepared).await
    }

    /// What "re-resolve tags" (ADR-0025) would do for `stack`: its active revision previewed
    /// again, so every third-party tag is resolved afresh. The diff is in the pins.
    #[tracing::instrument(skip(self))]
    pub async fn reresolve_preview(&self, stack: &str) -> Result<ApplyPreview> {
        let revision = self.active_revision(stack).await?;
        self.preview(&revision.toml).await
    }

    /// "Re-resolve tags" (ADR-0025, ADR-0049): resolves the third-party tags of `stack`'s
    /// active revision again and, if any digest changed, applies the result as a new revision
    /// and redeploys what changes, like any apply. `expected` is the revision the admin
    /// reviewed; a stack that moved on since is refused. `None` when every digest is unchanged:
    /// no revision is created and nothing redeploys.
    #[tracing::instrument(skip(self))]
    pub async fn reresolve(
        self: &Arc<Self>,
        stack: &str,
        expected: u32,
    ) -> Result<Option<ApplyOutcome>> {
        let revision = self.active_revision(stack).await?;
        let changed = |active: u32| OrchestratorError::RevisionChanged {
            stack: stack.to_owned(),
            expected,
            active,
        };
        if revision.revision != expected {
            return Err(changed(revision.revision));
        }
        // Resolving asks the registries and can take a while: look again afterwards, so an
        // apply that landed meanwhile is not reverted by re-storing the older file.
        let prepared = self.prepare(&revision.toml).await?;
        let latest = self.active_revision(stack).await?;
        if latest.revision != expected {
            return Err(changed(latest.revision));
        }
        if prepared.images == self.store.revision_images(revision.id).await? {
            return Ok(None);
        }
        self.store_and_redeploy(&revision.toml, prepared)
            .await
            .map(Some)
    }

    /// The active revision of `stack`, which must not be removed.
    async fn active_revision(&self, stack: &str) -> Result<StackRevision> {
        let unknown = || OrchestratorError::UnknownStack(stack.to_owned());
        if self.store.stack(stack).await?.is_none() {
            return Err(unknown());
        }
        self.store.latest_revision(stack).await?.ok_or_else(unknown)
    }

    /// Stores a prepared stack file as the active revision and redeploys what changes.
    async fn store_and_redeploy(
        self: &Arc<Self>,
        toml: &str,
        prepared: Prepared,
    ) -> Result<ApplyOutcome> {
        let Prepared {
            definition,
            images,
            warnings,
            changed,
            noop,
        } = prepared;
        let name = definition.name.to_string();
        let revision = self.store.add_pinned_revision(&name, toml, &images).await?;
        tracing::info!(stack = %name, revision = revision.revision, "applied stack revision");
        // Certificates are requested now, before any image is deployed (ADR-0036).
        self.lock().certs.apply(&definition);
        let deployment = if noop && !self.is_busy(&name) {
            None
        } else {
            let target = Target::new(definition, revision.clone(), images, DigestSet::new());
            Some(self.submit(target, DeploymentSource::Apply, None).await?)
        };
        Ok(ApplyOutcome {
            revision,
            warnings,
            changed,
            deployment,
        })
    }

    /// The checks an apply runs before it stores anything, and the plan they lead to.
    async fn prepare(&self, toml: &str) -> Result<Prepared> {
        let parsed = Stack::parse(toml)?;
        let definition = Arc::new(parsed.stack);
        if self.is_removing(definition.name.as_ref()) {
            return Err(OrchestratorError::Removing(definition.name.to_string()));
        }
        self.check_routes(&definition).await?;
        let images = self.resolve_images(&definition).await?;
        let running = self.store.services(definition.name.as_ref()).await?;
        let secrets = self.secret_versions(&definition).await?;
        let plan = Plan::new(&definition, &images, &DigestSet::new(), &running, &secrets)?;
        let changed = plan
            .changes
            .iter()
            .map(|c| c.service.to_string())
            .chain(plan.removed.iter().map(|p| p.service.clone()))
            .collect();
        let noop = plan.is_noop();
        Ok(Prepared {
            definition,
            images,
            warnings: parsed.warnings,
            changed,
            noop,
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
        Self::deployable(&definition, &images)?;
        let pins = self.store.revision_images(revision.id).await?;
        self.submit(
            Target::new(definition, revision, pins, images),
            source,
            None,
        )
        .await
    }

    /// A deploy from the webhook (ADR-0042, ADR-0047): the caller must match the stack's
    /// `[deploy]` rule, and every digest must be in its service's repository. Otherwise like
    /// [`deploy`](Self::deploy), with the provenance recorded on the deployment.
    ///
    /// An unknown stack is [`Forbidden`](OrchestratorError::Forbidden) too, so the answer does
    /// not tell anyone with a GitHub token which stacks exist.
    #[tracing::instrument(skip_all, fields(stack = %request.stack))]
    pub async fn deploy_webhook(self: &Arc<Self>, request: WebhookDeploy) -> Result<DeployOutcome> {
        let stack = request.stack.as_str();
        let forbidden = |reason: String| OrchestratorError::Forbidden {
            stack: stack.to_owned(),
            reason,
        };
        let revision = match self.store.stack(stack).await? {
            Some(_) => self.store.latest_revision(stack).await?,
            None => None,
        }
        .ok_or_else(|| forbidden("the stack does not exist".into()))?;
        let definition: Arc<Stack> = Arc::new(revision.toml.parse()?);
        let rule = definition
            .deploy
            .as_ref()
            .ok_or_else(|| forbidden("the stack file has no [deploy] section".into()))?;
        rule.check(&request.caller)
            .map_err(|denied| forbidden(denied.to_string()))?;
        Self::deployable(&definition, &request.images)?;
        for (service, digest) in &request.images {
            let image = &definition
                .services
                .iter()
                .find(|(name, _)| name.as_ref() == service)
                .ok_or_else(|| OrchestratorError::UnknownService(service.clone()))?
                .1
                .image;
            let digest: Digest = digest
                .parse()
                .map_err(|message| OrchestratorError::Digest {
                    service: service.clone(),
                    message,
                })?;
            match self.resolver.contains(image, &digest).await {
                Ok(()) => {}
                Err(RegistryError::NotFound { .. }) => {
                    return Err(OrchestratorError::ForeignDigest {
                        service: service.clone(),
                        repository: format!("{}/{}", image.registry(), image.repository()),
                    });
                }
                Err(err) => {
                    return Err(OrchestratorError::Resolve {
                        service: service.clone(),
                        message: err.to_string(),
                    });
                }
            }
        }
        let pins = self.store.revision_images(revision.id).await?;
        let target = Target::new(definition, revision, pins, request.images);
        self.submit(target, DeploymentSource::Webhook, Some(request.provenance))
            .await
    }

    /// Records a refused webhook attempt for the stack page (ADR-0047). Attempts on stacks
    /// hoster does not know are dropped.
    pub async fn record_rejection(&self, rejection: &WebhookRejection) -> Result<()> {
        self.store.record_rejection(rejection).await?;
        Ok(())
    }

    /// Checks that every service of `images` exists, is CI-built and gets a valid digest.
    fn deployable(definition: &Stack, images: &DigestSet) -> Result<()> {
        for (service, digest) in images {
            let image = definition
                .services
                .iter()
                .find(|(n, _)| n.as_ref() == service)
                .map(|(_, s)| &s.image)
                .ok_or_else(|| OrchestratorError::UnknownService(service.clone()))?;
            if !image.is_ci_built() {
                return Err(OrchestratorError::NotCiBuilt(service.clone()));
            }
            digest
                .parse::<Digest>()
                .map_err(|message| OrchestratorError::Digest {
                    service: service.clone(),
                    message,
                })?;
        }
        Ok(())
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
        // The old revision is the active one again, so its hosts are ensured and the ones only
        // the newer revision routed are forgotten, exactly as at apply (ADR-0036).
        self.lock().certs.apply(&definition);
        let target = Target::new(definition, revision, pins, earlier.target);
        self.submit(target, DeploymentSource::Rollback, None).await
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
            let mut state = self.lock();
            let slot = state.slots.entry(stack.to_owned()).or_default();
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
        self.reconciled(stack).await;
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
        {
            // The routes go with the slot, in one critical section.
            let mut state = self.lock();
            state.slots.remove(stack);
            state.routes.remove(&name);
            state.certs.remove(stack);
        }
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
            let mut state = self.lock();
            let slot = state.slots.entry(stack.to_string()).or_default();
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
        self.reconciled(stack.as_ref()).await;
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
                        let pointer = pointers.iter().find(|p| p.service == name.as_ref());
                        let restarts = pointer
                            .map(|p| self.restart_stats(&stack.name, &p.service, p.generation))
                            .unwrap_or_default();
                        let health = pointer
                            .map(|p| self.health_stats(&stack.name, &p.service, p.generation))
                            .unwrap_or_default();
                        let state = match pointer {
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
                            image: service.image.to_string(),
                            ci_built: service.image.is_ci_built(),
                            state,
                            restarts,
                            health,
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
                let state = self.lock();
                let slot = state.slots.get(&stack.name);
                (
                    slot.and_then(|s| s.active),
                    slot.and_then(|s| s.pending.as_ref().map(Run::id)),
                )
            };
            let rejections = self
                .store
                .rejections(&stack.name, Self::SHOWN_REJECTIONS)
                .await?;
            out.push(StackStatus {
                rejections,
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

    /// Deployment `id` of `stack`, with its state transitions in order.
    pub async fn deployment(
        &self,
        stack: &str,
        id: DeploymentId,
    ) -> Result<(Deployment, Vec<DeploymentEvent>)> {
        let deployment = self
            .store
            .deployment(id)
            .await?
            .filter(|d| d.stack == stack)
            .ok_or(OrchestratorError::UnknownDeployment(id))?;
        let events = self.store.events(id).await?;
        Ok((deployment, events))
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

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state is plain data, changed only in short non-awaiting sections; a panic cannot
        // leave it half-updated.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reconciler(&self) -> MutexGuard<'_, Reconciler> {
        // Restart histories are plain data; a panic cannot leave them half-updated.
        self.reconciler
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn watchlist(&self) -> MutexGuard<'_, monitor::Watchlist> {
        // Liveness records are plain data; a panic cannot leave them half-updated.
        self.watchlist
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn is_busy(&self, stack: &str) -> bool {
        self.lock()
            .slots
            .get(stack)
            .is_some_and(|s| s.active.is_some())
    }

    /// Takes `stack`'s slot for a reconcile pass, unless a deployment or a removal holds it.
    pub(super) fn reconcile_claim<'a>(
        self: &'a Arc<Self>,
        stack: &str,
    ) -> Option<ReconcileClaim<'a, R>> {
        let mut state = self.lock();
        let slot = state.slots.entry(stack.to_owned()).or_default();
        if slot.active.is_some() || slot.removing || slot.reconciling {
            return None;
        }
        slot.reconciling = true;
        Some(ReconcileClaim {
            orchestrator: self,
            stack: stack.to_owned(),
        })
    }

    /// Waits until no reconcile pass holds `stack`. Called once the stack is marked as being
    /// removed, so no new pass can take it.
    async fn reconciled(&self, stack: &str) {
        loop {
            let mut notified = pin!(self.finished.notified());
            notified.as_mut().enable();
            if !self.lock().slots.get(stack).is_some_and(|s| s.reconciling) {
                return;
            }
            notified.await;
        }
    }

    /// Whether the stack's removal has started.
    fn is_removing(&self, stack: &str) -> bool {
        self.lock().slots.get(stack).is_some_and(|s| s.removing)
    }

    /// Whether `id` is the active or pending deployment of `stack`.
    fn holds(&self, stack: &str, id: DeploymentId) -> bool {
        self.lock().slots.get(stack).is_some_and(|s| {
            s.active.is_some_and(|(a, _)| a == id)
                || s.pending.as_ref().is_some_and(|p| p.id() == id)
        })
    }

    /// Takes the stack's slot for `id` if it is free.
    fn claim(&self, stack: &str, id: DeploymentId, state: DeploymentState) -> Claim {
        let mut guard = self.lock();
        let slot = guard.slots.entry(stack.to_owned()).or_default();
        if slot.removing {
            Claim::Removing
        } else if slot.active.is_some() || slot.reconciling {
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
        let mut state = self.lock();
        let slot = state.slots.entry(stack.to_owned()).or_default();
        if slot.removing {
            Enqueued::Removing(run)
        } else if slot.active.is_none() && !slot.reconciling {
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
        provenance: Option<Provenance>,
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
                provenance,
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
            let mut state = self.lock();
            if let Ok(name) = stack.parse::<Name>() {
                // A deployment ends with no gap open; one left by a failed run closes here.
                state.routes.close_gaps(&name);
            }
            let slot = state.slots.entry(stack.to_owned()).or_default();
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

    /// The definitions of every stack except `name` whose routes are claimed: each one's active
    /// revision, and the definition its routes are served from now, if that is an older one
    /// (a route stays owned until the deployment that drops it commits).
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
        let state = self.lock();
        let served = state
            .routes
            .definitions()
            .filter(|served| served.name != *name && !others.contains(served))
            .cloned()
            .collect::<Vec<_>>();
        others.extend(served);
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
mod monitor_tests;
#[cfg(test)]
mod reconcile_tests;
#[cfg(test)]
mod routes_tests;
#[cfg(test)]
mod stack_secrets_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod zero_downtime_tests;
