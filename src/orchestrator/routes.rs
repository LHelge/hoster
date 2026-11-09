//! The orchestrator's in-memory desired state and the `RouteTable` it publishes from it
//! (architecture §6.4, ADR-0035 #1, ADR-0050).
//!
//! [`Routes`] lives inside the orchestrator's one `Mutex`, next to the deploy slots, so every
//! change to desired state and every `store` of a new table happens in a critical section that
//! never awaits. Each change rebuilds the whole table from the whole desired state: two stacks
//! deploying at once can never publish a table that lacks the other's routes.
//!
//! What is published:
//! - **committed state** ([`DesiredState`]): per stack, the definition its current generations
//!   were deployed from and each service's upstreams. Restored from the store at startup
//!   ([`Orchestrator::restore_routes`]), replaced per stack when a deployment reaches
//!   `Switching`, dropped when a stack is removed.
//! - **gaps**, overlaid on it: a stop-first service between stopping its old container and
//!   serving its new one answers 503 with `Retry-After` (ADR-0048). The overlay ends on the
//!   commit (the service is live on its new upstream) or on an abort (back to the old one).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;

use super::{Orchestrator, Result};
use crate::routing::{
    DesiredStack, DesiredState, Reserved, RouteClaims, RouteTable, ServiceState, Upstream,
};
use crate::runtime::ContainerRuntime;
use crate::stack::{Name, Stack};
use crate::store::{PortMap, ServicePointer, StackRevision};

/// How many deployments back [`Orchestrator::restore_routes`] looks for the one that committed
/// the current generations.
const COMMITTED_LOOKBACK: u32 = 200;

/// Desired state and the published table: the single writer of `ArcSwap<RouteTable>`.
#[derive(Debug)]
pub(super) struct Routes {
    table: Arc<ArcSwap<RouteTable>>,
    reserved: Reserved,
    /// How long a client is told to wait for a service in its stop-first gap.
    retry_after: Duration,
    committed: DesiredState,
    /// Stop-first services in their gap, by stack.
    gaps: BTreeMap<Name, BTreeSet<Name>>,
}

/// One changed service of a committing deployment: its new upstreams by container port.
pub(super) type Live = (Name, BTreeMap<u16, Arc<Upstream>>);

impl Routes {
    pub(super) fn new(
        table: Arc<ArcSwap<RouteTable>>,
        reserved: Reserved,
        retry_after: Duration,
    ) -> Self {
        Self {
            table,
            reserved,
            retry_after,
            committed: DesiredState::default(),
            gaps: BTreeMap::new(),
        }
    }

    /// The definitions the current generations of every stack were deployed from, which the
    /// published routes come from.
    pub(super) fn definitions(&self) -> impl Iterator<Item = &Stack> {
        self.committed.stacks().map(|d| d.stack().as_ref())
    }

    /// Replaces the whole desired state (startup) and publishes it. Returns the version now
    /// published.
    ///
    /// A stack whose routes collide with a stack restored before it, or with a reserved host,
    /// is left out with an error, so the table always builds: the other stacks and the admin
    /// host are served whatever the store holds.
    pub(super) fn restore(&mut self, stacks: impl IntoIterator<Item = DesiredStack>) -> u64 {
        self.committed = DesiredState::default();
        self.gaps.clear();
        for stack in stacks {
            match RouteClaims::check(stack.stack(), self.definitions(), &self.reserved) {
                Ok(()) => {
                    self.committed.insert(stack);
                }
                Err(err) => tracing::error!(
                    stack = %stack.stack().name,
                    %err,
                    "stored routes collide; the stack is not routed"
                ),
            }
        }
        self.publish("restored desired state")
            .unwrap_or_else(|| self.table.load().version())
    }

    /// A deployment of `definition` reached `Switching`: every service in `live` serves from
    /// its new upstreams, every service in `removed` is gone, the others keep theirs. Ends the
    /// stack's gaps and publishes.
    ///
    /// Returns what the stack served before, for [`revert`](Self::revert) should the pointers
    /// fail to commit.
    pub(super) fn commit(
        &mut self,
        definition: Arc<Stack>,
        live: impl IntoIterator<Item = Live>,
        removed: &[Name],
    ) -> Option<DesiredStack> {
        let name = definition.name.clone();
        let before = self.committed.get(&name).cloned();
        let mut stack = DesiredStack::new(definition);
        if let Some(before) = &before {
            stack.services = before.services.clone();
        }
        for service in removed {
            stack.services.remove(service);
        }
        for (service, upstreams) in live {
            stack
                .services
                .insert(service, ServiceState::Live { upstreams });
        }
        let reason = format!("deployment of stack {name} committed");
        self.update(&reason, |committed, gaps| {
            committed.insert(stack);
            gaps.remove(&name);
        });
        before
    }

    /// Undoes a [`commit`](Self::commit) whose pointers were not written: `stack` serves what
    /// it served `before` again (nothing, after a first deployment), as its pointers still say.
    pub(super) fn revert(&mut self, stack: &Name, before: Option<DesiredStack>) {
        self.update(
            &format!("commit of stack {stack} reverted"),
            |committed, gaps| {
                committed.remove(stack);
                if let Some(before) = before {
                    committed.insert(before);
                }
                gaps.remove(stack);
            },
        );
    }

    /// Puts `stack`'s `service` in its stop-first gap and publishes.
    pub(super) fn open_gap(&mut self, stack: &Name, service: &Name) {
        let reason = format!("service {service} of stack {stack} in its gap");
        self.update(&reason, |_, gaps| {
            gaps.entry(stack.clone())
                .or_default()
                .insert(service.clone());
        });
    }

    /// Ends every gap of `stack` (an abort restores the old upstreams); publishes if there was
    /// one.
    pub(super) fn close_gaps(&mut self, stack: &Name) {
        if self.gaps.contains_key(stack) {
            self.update(&format!("gaps of stack {stack} closed"), |_, gaps| {
                gaps.remove(stack);
            });
        }
    }

    /// Drops `stack`'s routes and publishes.
    pub(super) fn remove(&mut self, stack: &Name) {
        self.update(&format!("stack {stack} removed"), |committed, gaps| {
            committed.remove(stack);
            gaps.remove(stack);
        });
    }

    /// Applies `change` to the committed state and the gaps, then publishes. A change whose
    /// table does not build is undone, so the state always builds and one bad change never
    /// blocks the publishes of every later one.
    fn update(
        &mut self,
        reason: &str,
        change: impl FnOnce(&mut DesiredState, &mut BTreeMap<Name, BTreeSet<Name>>),
    ) {
        let committed = self.committed.clone();
        let gaps = self.gaps.clone();
        change(&mut self.committed, &mut self.gaps);
        if self.publish(reason).is_none() {
            self.committed = committed;
            self.gaps = gaps;
        }
    }

    /// The loopback upstreams of `stack`'s `service` at `generation`, by container port.
    pub(super) fn loopback(
        stack: &Name,
        service: &Name,
        generation: u32,
        ports: &PortMap,
    ) -> BTreeMap<u16, Arc<Upstream>> {
        ports
            .iter()
            .map(|(&container, &host)| {
                let upstream = Upstream::loopback(host, stack.clone(), service.clone(), generation);
                (container, Arc::new(upstream))
            })
            .collect()
    }

    /// The committed state with the gaps laid over it.
    fn desired(&self) -> DesiredState {
        let mut desired = self.committed.clone();
        for (stack, services) in &self.gaps {
            if let Some(stack) = desired.get_mut(stack) {
                for service in services {
                    stack.services.insert(
                        service.clone(),
                        ServiceState::Gap {
                            retry_after: self.retry_after,
                        },
                    );
                }
            }
        }
        desired
    }

    /// Builds the table from desired state and stores it, logging the version and what
    /// changed. Returns the version now published, or `None` when the table does not build.
    ///
    /// A build that fails the collision check (the last line of defense, ADR-0012) keeps the
    /// previous table: validation rejects colliding stacks before they get this far.
    fn publish(&self, reason: &str) -> Option<u64> {
        let previous = self.table.load();
        match RouteTable::build(&self.desired(), &self.reserved) {
            Ok(table) => {
                let diff = table.diff(&previous);
                let version = table.version();
                tracing::info!(version, reason, %diff, "published route table");
                self.table.store(Arc::new(table));
                Some(version)
            }
            Err(err) => {
                tracing::error!(
                    version = previous.version(),
                    reason,
                    %err,
                    "route table does not build; keeping the published one"
                );
                None
            }
        }
    }
}

impl<R: ContainerRuntime> Orchestrator<R> {
    /// Rebuilds desired state from the store and publishes the `RouteTable` (architecture §4
    /// step 6): for every stack with running services, the definition of the deployment that
    /// committed them, with each service's current generation on its loopback ports. Returns
    /// the published version.
    ///
    /// Call once at startup, after [`recover`](Self::recover) and before the proxy starts and
    /// any deployment is submitted: it replaces the in-memory state wholesale.
    pub async fn restore_routes(&self) -> Result<u64> {
        let mut desired = Vec::new();
        let mut applied = Vec::new();
        for stack in self.store.stacks().await? {
            // Certificates follow the applied revision, as they do at every apply (ADR-0036),
            // whether or not it has been deployed yet.
            if let Some(definition) = self.applied_definition(&stack.name).await? {
                applied.push(definition);
            }
            let pointers = self.store.services(&stack.name).await?;
            if pointers.is_empty() {
                // Nothing runs, so nothing is routed.
                continue;
            }
            let Some(definition) = self.committed_definition(&stack.name).await? else {
                continue;
            };
            desired.push(Self::desired_stack(definition, &pointers));
        }
        let mut state = self.lock();
        let version = state.routes.restore(desired);
        state.certs.restore(applied.iter().map(Arc::as_ref));
        Ok(version)
    }

    /// The definition of `stack`'s latest (active) revision.
    async fn applied_definition(&self, stack: &str) -> Result<Option<Arc<Stack>>> {
        let revision = self.store.latest_revision(stack).await?;
        Ok(revision.and_then(Self::parse_revision))
    }

    /// `revision`'s stack file, parsed; `None` (logged) when it no longer parses.
    fn parse_revision(revision: StackRevision) -> Option<Arc<Stack>> {
        match revision.toml.parse::<Stack>() {
            Ok(definition) => Some(Arc::new(definition)),
            Err(err) => {
                tracing::warn!(stack = revision.stack, revision = revision.revision, %err, "stored revision no longer parses; its routes are not served");
                None
            }
        }
    }

    /// The definition of `stack`'s most recent committed deployment, or its active revision
    /// when none is recorded.
    pub(super) async fn committed_definition(&self, stack: &str) -> Result<Option<Arc<Stack>>> {
        let revision = match self.committed_revision(stack).await? {
            Some(revision) => Some(revision),
            None => self.store.latest_revision(stack).await?,
        };
        Ok(revision.and_then(Self::parse_revision))
    }

    /// The revision of `stack`'s most recent committed deployment, if one is recorded.
    pub(super) async fn committed_revision(&self, stack: &str) -> Result<Option<StackRevision>> {
        let committed = self
            .store
            .deployments(stack, COMMITTED_LOOKBACK)
            .await?
            .into_iter()
            .find(|d| d.committed_at.is_some())
            .and_then(|d| d.revision);
        Ok(match committed {
            Some(id) => self.store.revision(id).await?,
            None => None,
        })
    }

    /// `definition` with every service in `pointers` live on its current generation.
    fn desired_stack(definition: Arc<Stack>, pointers: &[ServicePointer]) -> DesiredStack {
        let name = definition.name.clone();
        let mut stack = DesiredStack::new(definition);
        for pointer in pointers {
            let Ok(service) = pointer.service.parse::<Name>() else {
                tracing::warn!(stack = %name, service = pointer.service, "invalid service name in a pointer");
                continue;
            };
            let upstreams = Routes::loopback(&name, &service, pointer.generation, &pointer.ports);
            stack = stack.with_live(service, upstreams);
        }
        stack
    }
}
