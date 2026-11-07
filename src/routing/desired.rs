//! The input of `RouteTable::build`: for each stack, its definition and what each of its
//! services currently runs (architecture §6.4).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use super::upstream::Upstream;
use crate::stack::{Name, Stack};

/// The routes hoster should serve: every stack with the current state of its services. The
/// orchestrator owns one and rebuilds the `RouteTable` from it after every change.
#[derive(Debug, Clone, Default)]
pub struct DesiredState {
    stacks: BTreeMap<Name, DesiredStack>,
}

impl DesiredState {
    /// Adds or replaces a stack, returning the version it replaced.
    pub fn insert(&mut self, stack: DesiredStack) -> Option<DesiredStack> {
        self.stacks.insert(stack.stack.name.clone(), stack)
    }

    /// Removes a stack; its routes disappear from the next table.
    pub fn remove(&mut self, name: &Name) -> Option<DesiredStack> {
        self.stacks.remove(name)
    }

    pub fn get(&self, name: &Name) -> Option<&DesiredStack> {
        self.stacks.get(name)
    }

    pub fn get_mut(&mut self, name: &Name) -> Option<&mut DesiredStack> {
        self.stacks.get_mut(name)
    }

    /// The stacks in name order.
    pub fn stacks(&self) -> impl Iterator<Item = &DesiredStack> {
        self.stacks.values()
    }
}

impl FromIterator<DesiredStack> for DesiredState {
    fn from_iter<I: IntoIterator<Item = DesiredStack>>(iter: I) -> Self {
        let mut state = Self::default();
        iter.into_iter().for_each(|stack| {
            state.insert(stack);
        });
        state
    }
}

/// One stack: its applied definition, whose routes are served, and the state of each service.
///
/// A service with routes but no entry in `services` (never deployed) gets no routes; neither
/// does a route whose container port has no upstream.
///
/// The definition is fixed at construction because [`DesiredState`] keys the stack by its name;
/// replacing it means inserting a new `DesiredStack`.
#[derive(Debug, Clone)]
pub struct DesiredStack {
    stack: Arc<Stack>,
    pub services: BTreeMap<Name, ServiceState>,
}

impl DesiredStack {
    /// A stack none of whose services runs yet.
    pub fn new(stack: impl Into<Arc<Stack>>) -> Self {
        Self {
            stack: stack.into(),
            services: BTreeMap::new(),
        }
    }

    /// The applied stack definition whose routes are served.
    pub fn stack(&self) -> &Arc<Stack> {
        &self.stack
    }

    /// Sets `service` live on `upstreams`, given as (container port, upstream) pairs.
    pub fn with_live(
        mut self,
        service: Name,
        upstreams: impl IntoIterator<Item = (u16, Arc<Upstream>)>,
    ) -> Self {
        let upstreams = upstreams.into_iter().collect();
        self.services
            .insert(service, ServiceState::Live { upstreams });
        self
    }

    /// Puts `service` in its stop-first gap (ADR-0039, ADR-0048).
    pub fn with_gap(mut self, service: Name, retry_after: Duration) -> Self {
        self.services
            .insert(service, ServiceState::Gap { retry_after });
        self
    }
}

/// What one service of a stack currently serves from.
#[derive(Debug, Clone)]
pub enum ServiceState {
    /// The current generation runs; container port → its loopback upstream.
    Live {
        upstreams: BTreeMap<u16, Arc<Upstream>>,
    },
    /// A stop-first service between stopping its old and serving its new container: its routes
    /// answer 503 with `Retry-After`.
    Gap { retry_after: Duration },
}
