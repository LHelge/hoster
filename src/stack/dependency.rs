//! The dependency graph between a stack's services (ADR-0056).
//!
//! Validation uses [`Graph`] over the raw service keys to find cycles; a validated [`Stack`]
//! exposes it through [`Stack::dependencies`], [`Stack::transitive_dependencies`] and
//! [`Stack::dependency_order`], so the engine, `Plan` and the reconciler never re-derive it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{Name, Stack};

impl Stack {
    /// The services `service` names in `depends_on`, in name order. Empty for an unknown service.
    pub fn dependencies(&self, service: &Name) -> impl Iterator<Item = &Name> {
        self.services
            .get(service)
            .into_iter()
            .flat_map(|s| s.depends_on.iter())
    }

    /// Every service `service` depends on, directly or through other services.
    pub fn transitive_dependencies(&self, service: &Name) -> BTreeSet<&Name> {
        self.graph().transitive(service)
    }

    /// Every service, each after all of its dependencies, with name as tie-break: the order of
    /// the replace phase and of restarts (ADR-0056 Decision 2). Filtering it to a subset keeps
    /// every dependency ahead of its dependents. It is an order, not a set of waves: a service
    /// may start as soon as its own dependencies have passed.
    pub fn dependency_order(&self) -> Vec<&Name> {
        self.graph().order()
    }

    /// Sorts `items` by their service `name` in [`Self::dependency_order`], name as tie-break;
    /// a service the stack does not have comes last. Edges to services not among `items`
    /// only order, they never hold anything back.
    pub fn sort_in_dependency_order<T>(&self, items: &mut [T], name: impl Fn(&T) -> &Name) {
        let order = self.dependency_order();
        items.sort_by_cached_key(|item| {
            let name = name(item);
            let at = order.iter().position(|o| *o == name);
            (at.unwrap_or(usize::MAX), name.clone())
        });
    }

    fn graph(&self) -> Graph<'_, Name> {
        Graph::new(
            self.services
                .iter()
                .map(|(name, service)| (name, &service.depends_on)),
        )
    }
}

/// Each node's dependencies. Edges to nodes outside the graph are dropped.
pub(super) struct Graph<'a, N: ?Sized> {
    edges: BTreeMap<&'a N, BTreeSet<&'a N>>,
}

impl<'a, N: Ord + ?Sized> Graph<'a, N> {
    pub(super) fn new<D>(nodes: impl IntoIterator<Item = (&'a N, D)>) -> Self
    where
        D: IntoIterator<Item = &'a N>,
    {
        let edges: BTreeMap<&'a N, D> = nodes.into_iter().collect();
        let known: BTreeSet<&'a N> = edges.keys().copied().collect();
        let edges = edges
            .into_iter()
            .map(|(node, deps)| {
                let deps = deps.into_iter().filter(|d| known.contains(d)).collect();
                (node, deps)
            })
            .collect();
        Self { edges }
    }

    fn direct(&self, node: &N) -> impl Iterator<Item = &'a N> + '_ {
        self.edges.get(node).into_iter().flatten().copied()
    }

    pub(super) fn transitive(&self, node: &N) -> BTreeSet<&'a N> {
        let mut seen = BTreeSet::new();
        let mut todo: Vec<&'a N> = self.direct(node).collect();
        while let Some(next) = todo.pop() {
            if seen.insert(next) {
                todo.extend(self.direct(next));
            }
        }
        seen
    }

    /// Kahn's algorithm, taking the smallest ready node first. Nodes on a cycle, which a
    /// validated stack has none of, come last in name order.
    pub(super) fn order(&self) -> Vec<&'a N> {
        let mut waiting: BTreeMap<&'a N, usize> = BTreeMap::new();
        let mut dependents: BTreeMap<&'a N, Vec<&'a N>> = BTreeMap::new();
        for (&node, deps) in &self.edges {
            waiting.insert(node, deps.len());
            for &dep in deps {
                dependents.entry(dep).or_default().push(node);
            }
        }
        let mut ready: BTreeSet<&'a N> = waiting
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(node, _)| *node)
            .collect();
        let mut order = Vec::with_capacity(self.edges.len());
        while let Some(node) = ready.pop_first() {
            waiting.remove(node);
            order.push(node);
            for &dependent in dependents.get(node).into_iter().flatten() {
                if let Some(count) = waiting.get_mut(dependent) {
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(dependent);
                    }
                }
            }
        }
        order.extend(waiting.into_keys());
        order
    }

    /// One cycle per strongly connected component that has one: the shortest path from the
    /// component's smallest node back to it, listed without repeating that node.
    pub(super) fn cycles(&self) -> Vec<Vec<&'a N>> {
        let reach: BTreeMap<&'a N, BTreeSet<&'a N>> = self
            .edges
            .keys()
            .map(|&node| (node, self.transitive(node)))
            .collect();
        let mut covered = BTreeSet::new();
        let mut cycles = Vec::new();
        for (&start, reachable) in &reach {
            if covered.contains(start) || !reachable.contains(start) {
                continue;
            }
            let component: BTreeSet<&'a N> = reachable
                .iter()
                .copied()
                .filter(|node| reach[node].contains(start))
                .collect();
            cycles.push(self.shortest_cycle(start, &component));
            covered.extend(component);
        }
        cycles
    }

    /// Breadth-first from `start` within `component`, neighbours in name order.
    fn shortest_cycle(&self, start: &'a N, component: &BTreeSet<&'a N>) -> Vec<&'a N> {
        let mut parent: BTreeMap<&'a N, &'a N> = BTreeMap::new();
        let mut queue = VecDeque::from([start]);
        while let Some(node) = queue.pop_front() {
            for next in self.direct(node).filter(|n| component.contains(n)) {
                if next == start {
                    let mut path = vec![node];
                    let mut current = node;
                    while current != start {
                        current = parent[current];
                        path.push(current);
                    }
                    path.reverse();
                    return path;
                }
                if !parent.contains_key(next) {
                    parent.insert(next, node);
                    queue.push_back(next);
                }
            }
        }
        // `start` is in its own component, so the loop above always returns.
        vec![start]
    }
}
