//! The reconciler (ADR-0028): makes podman match the persisted per-service pointers.
//!
//! - **At startup** ([`Orchestrator::recover`], architecture §4 step 5) it resolves every
//!   deployment a crash interrupted (§5.4), adopts the running containers of each service's
//!   current generation, starts the stopped ones, removes every other generation, removes the
//!   containers and networks of stacks that no longer exist, and releases port reservations
//!   nothing holds any more.
//! - **While hoster runs** ([`Orchestrator::run_reconciler`]) it repeats the container part
//!   about every 15 s, restarting exited containers of the current generation with exponential
//!   backoff (1 s → 5 min) and marking a service *crash looping* after 5 restarts in 10 minutes.
//!   Stacks with an active deployment are skipped: the deployment owns their containers.
//!
//! The per-service current pointers decide everything (§5.4): a container of the generation a
//! pointer names is kept, any other generation of a known stack is removed. A container whose
//! `hoster.*` labels are missing or do not match its name is not hoster's; it is left alone
//! and warned about once.
//!
//! A pass works on one stack at a time and holds that stack's deploy slot meanwhile
//! ([`Orchestrator::reconcile_claim`]): a stack with an active deployment or a removal is
//! skipped, and a deployment submitted during the pass waits until the stack is done. Only with
//! the slot held does the pass list the stack's containers and read its pointers, so it never
//! acts on a view a deployment changed, e.g. by restarting the old container of a stop-first
//! service the deployment is replacing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{Orchestrator, Result};
use crate::runtime::{
    ContainerInfo, ContainerRuntime, ContainerState, NetworkSpec, ObjectKind, RuntimeError,
    ServiceGeneration, label,
};
use crate::stack::{DEFAULT_STOP_TIMEOUT, Name};
use crate::store::{Deployment, DeploymentId, DeploymentState, ServicePointer};

/// How the reconciler restarts crashed containers (ADR-0028).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// How often the reconcile loop runs.
    pub interval: Duration,
    /// The wait before the second restart of a container; it doubles with every restart.
    pub initial_backoff: Duration,
    /// The longest wait between restarts, and the wait while crash looping.
    pub max_backoff: Duration,
    /// This many restarts within [`Self::crash_loop_window`] mark the service crash looping.
    pub crash_loop_restarts: usize,
    pub crash_loop_window: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(15),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(5 * 60),
            crash_loop_restarts: 5,
            crash_loop_window: Duration::from_secs(10 * 60),
        }
    }
}

/// The restart history of one container generation: the backoff state machine of ADR-0028.
///
/// The first restart happens as soon as the container is seen exited; each later one waits
/// the current backoff after the previous one, which doubles up to the maximum. Once the
/// restarts within the window reach the threshold the service is crash looping, and every
/// further restart waits the maximum. The history resets when the container is seen running
/// with no restart left in the window, i.e. it stayed up for a whole window.
#[derive(Debug, Clone)]
pub(super) struct Restarts {
    policy: RestartPolicy,
    /// Restarts within the window, oldest first.
    recent: VecDeque<Instant>,
    /// Every restart of this generation, for the UI.
    total: u32,
    /// The wait after the next restart.
    backoff: Duration,
    /// No restart before this.
    not_before: Option<Instant>,
    crash_looping: bool,
}

impl Restarts {
    pub(super) fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            recent: VecDeque::new(),
            total: 0,
            backoff: policy.initial_backoff,
            not_before: None,
            crash_looping: false,
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .recent
            .front()
            .is_some_and(|&at| now.saturating_duration_since(at) >= self.policy.crash_loop_window)
        {
            self.recent.pop_front();
        }
    }

    /// Whether the backoff allows a restart at `now`.
    pub(super) fn is_due(&self, now: Instant) -> bool {
        self.not_before.is_none_or(|at| now >= at)
    }

    /// When the next restart is allowed, if it is still ahead of `now`.
    pub(super) fn due_after(&self, now: Instant) -> Option<Instant> {
        self.not_before.filter(|&at| at > now)
    }

    /// Records a restart (attempt) at `now`. Returns whether the service just started crash
    /// looping.
    pub(super) fn record(&mut self, now: Instant) -> bool {
        self.prune(now);
        self.recent.push_back(now);
        self.total += 1;
        let was = self.crash_looping;
        if self.recent.len() >= self.policy.crash_loop_restarts {
            self.crash_looping = true;
        }
        let wait = if self.crash_looping {
            self.policy.max_backoff
        } else {
            self.backoff
        };
        self.not_before = Some(now + wait);
        self.backoff = (self.backoff * 2).min(self.policy.max_backoff);
        self.crash_looping && !was
    }

    /// The container was seen running at `now`. Returns whether that cleared its history.
    pub(super) fn running(&mut self, now: Instant) -> bool {
        self.prune(now);
        if !self.recent.is_empty() || self.not_before.is_none() {
            return false;
        }
        let policy = self.policy;
        let total = self.total;
        *self = Self::new(policy);
        self.total = total;
        true
    }

    pub(super) fn is_crash_looping(&self) -> bool {
        self.crash_looping
    }

    /// The wait before the restart after the next one: what the log reports.
    fn backoff(&self) -> Duration {
        self.backoff
    }
}

/// What the reconciler remembers between passes.
#[derive(Debug, Default)]
pub(super) struct Reconciler {
    /// Restart histories of current-generation containers.
    restarts: BTreeMap<ServiceGeneration, Restarts>,
    /// What was already warned about (foreign objects, missing containers), so the loop does
    /// not repeat it every pass.
    warned: BTreeSet<String>,
}

impl Reconciler {
    /// Whether `key` is new, remembering it.
    fn first_warning(&mut self, key: String) -> bool {
        self.warned.insert(key)
    }
}

/// A service's restart statistics, as [`Orchestrator::status`] reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestartStats {
    /// Restarts of the current generation since hoster started.
    pub restarts: u32,
    /// 5 restarts within 10 minutes (ADR-0028).
    pub crash_looping: bool,
}

/// Which pass is running: the startup one starts stopped containers at once and resolves
/// interrupted deployments; the periodic one restarts with backoff and skips busy stacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pass {
    Startup,
    Periodic,
}

/// What one reconcile pass did, by container or network name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Deployments a crash interrupted, with the state they were resolved to (§5.4).
    pub recovered: Vec<(DeploymentId, DeploymentState)>,
    /// Running containers of a current generation.
    pub adopted: Vec<String>,
    /// Stopped containers of a current generation that were started (startup) or restarted.
    pub started: Vec<String>,
    /// Exited containers whose restart waits for the backoff.
    pub backing_off: Vec<String>,
    /// Containers of old or aborted generations, or of removed stacks.
    pub removed: Vec<String>,
    /// Networks of removed stacks.
    pub networks_removed: Vec<String>,
    /// Port reservations nothing holds any more (startup only).
    pub ports_released: Vec<u16>,
    /// Stacks skipped because a deployment is active.
    pub skipped_stacks: Vec<String>,
    /// Objects with `hoster.managed=true` whose other labels are missing or do not match.
    pub foreign: Vec<String>,
    /// Actions that failed; retried next pass.
    pub problems: Vec<String>,
}

impl<R: ContainerRuntime> Orchestrator<R> {
    /// Startup reconciliation (architecture §4 step 5): resolves interrupted deployments
    /// (§5.4), then brings the containers, networks and port reservations in line with the
    /// per-service pointers. Call once, before any deployment is submitted.
    pub async fn recover(self: &Arc<Self>) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();
        for deployment in self.store.unfinished_deployments().await? {
            if self.holds(&deployment.stack, deployment.id) {
                continue;
            }
            let to = self.resolve_interrupted(&deployment).await?;
            report.recovered.push((deployment.id, to));
        }
        self.pass(Pass::Startup, Instant::now(), &mut report)
            .await?;
        self.release_stale_ports(&mut report).await?;
        tracing::info!(
            recovered = report.recovered.len(),
            adopted = report.adopted.len(),
            started = report.started.len(),
            removed = report.removed.len(),
            networks_removed = report.networks_removed.len(),
            ports_released = report.ports_released.len(),
            foreign = report.foreign.len(),
            problems = report.problems.len(),
            "startup reconciliation done"
        );
        Ok(report)
    }

    /// One periodic pass: what the reconcile loop runs.
    pub async fn reconcile(self: &Arc<Self>) -> Result<ReconcileReport> {
        self.reconcile_at(Instant::now()).await
    }

    /// [`Self::reconcile`] as if it ran at `now`.
    pub(super) async fn reconcile_at(self: &Arc<Self>, now: Instant) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();
        self.pass(Pass::Periodic, now, &mut report).await?;
        Ok(report)
    }

    /// The reconcile loop (ADR-0028): a pass every [`RestartPolicy::interval`], and early when
    /// a backed-off restart falls due. Ends when `cancel` fires.
    pub async fn run_reconciler(self: Arc<Self>, cancel: CancellationToken) {
        let interval = self.settings.restart.interval;
        let mut tick = Instant::now() + interval;
        loop {
            let wake = self
                .next_restart(Instant::now())
                .map_or(tick, |at| at.min(tick));
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep_until(wake) => {}
            }
            let now = Instant::now();
            if now >= tick {
                tick = now + interval;
            }
            if let Err(err) = self.reconcile_at(now).await {
                tracing::warn!(%err, "reconcile pass failed; retrying at the next one");
            }
        }
        tracing::debug!("reconcile loop stopped");
    }

    /// The restart statistics of `stack`'s `service` at `generation`.
    pub(super) fn restart_stats(
        &self,
        stack: &str,
        service: &str,
        generation: u32,
    ) -> RestartStats {
        let reconciler = self.reconciler();
        reconciler
            .restarts
            .iter()
            .find(|(key, _)| {
                key.stack.as_ref() == stack
                    && key.service.as_ref() == service
                    && key.generation == generation
            })
            .map(|(_, r)| RestartStats {
                restarts: r.total,
                crash_looping: r.is_crash_looping(),
            })
            .unwrap_or_default()
    }

    /// The earliest backed-off restart still ahead of `now`.
    fn next_restart(&self, now: Instant) -> Option<Instant> {
        self.reconciler()
            .restarts
            .values()
            .filter_map(|r| r.due_after(now))
            .min()
    }

    /// Resolves one interrupted deployment by its persisted state (§5.4). The containers are
    /// sorted out by the pass that follows: an uncommitted deployment's generation is not
    /// current, so its containers are removed and the old generation's started; a committed
    /// one's pointers are authoritative, so the previous generation is removed.
    async fn resolve_interrupted(&self, deployment: &Deployment) -> Result<DeploymentState> {
        let (to, message) = if deployment.committed_at.is_some() {
            (
                DeploymentState::Live,
                "recovered after a hoster restart: the committed pointers are authoritative",
            )
        } else {
            (DeploymentState::Failed, "interrupted by a hoster restart")
        };
        tracing::warn!(
            stack = %deployment.stack,
            deployment_id = %deployment.id,
            generation = deployment.generation,
            from = %deployment.state,
            to = %to,
            "resolving interrupted deployment"
        );
        self.store
            .transition(deployment.id, to, Some(message))
            .await?;
        Ok(to)
    }

    /// One pass over every stack the store or podman knows.
    async fn pass(
        self: &Arc<Self>,
        pass: Pass,
        now: Instant,
        report: &mut ReconcileReport,
    ) -> Result<()> {
        // Find the stacks, and the objects that are not hoster's. What to do with each stack's
        // objects is decided from a fresh listing once its slot is held.
        let mut containers = self.runtime.list_managed().await?;
        containers.sort_by(|a, b| a.name.cmp(&b.name));
        let networks = self.runtime.list_managed_networks().await?;
        let mut stacks: BTreeSet<String> = self
            .store
            .stacks()
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect();
        for container in &containers {
            match ServiceGeneration::try_from(container) {
                Ok(generation) => {
                    stacks.insert(generation.stack.to_string());
                }
                Err(reason) => self.foreign("container", &container.name, &reason, report),
            }
        }
        let mut stack_networks = BTreeMap::new();
        for network in networks {
            match network_stack(&network) {
                Ok(stack) => {
                    stacks.insert(stack.to_string());
                    stack_networks.insert(stack.to_string(), network);
                }
                Err(reason) => self.foreign("network", &network.name, &reason, report),
            }
        }

        // Restart histories of the generations still current after this pass.
        let mut current = BTreeSet::new();
        let mut skipped = BTreeSet::new();
        for stack in stacks {
            let Some(_claim) = self.reconcile_claim(&stack) else {
                tracing::debug!(
                    stack,
                    "a deployment or removal holds the stack; not reconciling it"
                );
                report.skipped_stacks.push(stack.clone());
                skipped.insert(stack);
                continue;
            };
            self.pass_stack(
                pass,
                now,
                &stack,
                stack_networks.get(&stack),
                &mut current,
                report,
            )
            .await?;
        }
        self.reconciler()
            .restarts
            .retain(|key, _| current.contains(key) || skipped.contains(key.stack.as_ref()));
        Ok(())
    }

    /// Reconciles one stack whose slot the caller holds. Adds the current generations it saw
    /// to `current`.
    async fn pass_stack(
        &self,
        pass: Pass,
        now: Instant,
        stack: &str,
        network: Option<&NetworkSpec>,
        current: &mut BTreeSet<ServiceGeneration>,
        report: &mut ReconcileReport,
    ) -> Result<()> {
        let mut containers: Vec<(ServiceGeneration, ContainerInfo)> = self
            .runtime
            .list_managed()
            .await?
            .into_iter()
            .filter_map(|container| {
                let generation = ServiceGeneration::try_from(&container).ok()?;
                (generation.stack.as_ref() == stack).then_some((generation, container))
            })
            .collect();
        containers.sort_by(|a, b| a.1.name.cmp(&b.1.name));

        if self.store.stack(stack).await?.is_none() {
            for (generation, container) in &containers {
                self.retire(generation, container, "its stack was removed", report)
                    .await;
            }
            if let Some(network) = network {
                self.collect_network(stack, network, report).await;
            }
            return Ok(());
        }

        let pointers = self.store.services(stack).await?;
        let (keep, old): (Vec<_>, Vec<_>) = containers
            .iter()
            .partition(|(generation, _)| is_current(&pointers, generation));
        // Old generations go first, so a stop-first service never runs twice at once, e.g.
        // when a crash left its aborted new generation running and the old one stopped.
        for (generation, container) in old {
            self.retire(generation, container, "not a current generation", report)
                .await;
        }
        for (generation, container) in keep {
            current.insert(generation.clone());
            self.keep_running(pass, now, generation, container, report)
                .await;
        }
        self.warn_missing(stack, &pointers, &containers);
        Ok(())
    }

    /// Keeps a current-generation container running: adopts it, or (re)starts it.
    async fn keep_running(
        &self,
        pass: Pass,
        now: Instant,
        generation: &ServiceGeneration,
        container: &ContainerInfo,
        report: &mut ReconcileReport,
    ) {
        let (stack, service, gen_no) = fields(generation);
        if container.state.is_running() {
            if let Some(restarts) = self.reconciler().restarts.get_mut(generation)
                && restarts.running(now)
            {
                tracing::info!(
                    stack,
                    service,
                    generation = gen_no,
                    "container stayed up; restart backoff reset"
                );
            }
            report.adopted.push(container.name.clone());
            return;
        }
        if container.state == ContainerState::Removing {
            return;
        }
        if pass == Pass::Startup {
            tracing::info!(
                stack, service, generation = gen_no, container = %container.name,
                state = %container.state, "starting stopped container of the current generation"
            );
            match self.runtime.start(&container.id).await {
                Ok(()) => report.started.push(container.name.clone()),
                Err(err) => self.problem(
                    generation,
                    format!("cannot start {}: {err}", container.name),
                    report,
                ),
            }
            return;
        }
        let due = {
            let mut reconciler = self.reconciler();
            let policy = self.settings.restart;
            reconciler
                .restarts
                .entry(generation.clone())
                .or_insert_with(|| Restarts::new(policy))
                .is_due(now)
        };
        if !due {
            report.backing_off.push(container.name.clone());
            return;
        }
        let started = self.runtime.start(&container.id).await;
        let (looping, total, backoff) = {
            let mut reconciler = self.reconciler();
            let policy = self.settings.restart;
            let restarts = reconciler
                .restarts
                .entry(generation.clone())
                .or_insert_with(|| Restarts::new(policy));
            let looping = restarts.record(now);
            (looping, restarts.total, restarts.backoff())
        };
        match started {
            Ok(()) => {
                tracing::warn!(
                    stack, service, generation = gen_no, container = %container.name,
                    state = %container.state, restarts = total, next_backoff = ?backoff,
                    "restarted exited container"
                );
                report.started.push(container.name.clone());
            }
            Err(err) => self.problem(
                generation,
                format!("cannot restart {}: {err}", container.name),
                report,
            ),
        }
        if looping {
            tracing::error!(
                stack, service, generation = gen_no,
                restarts = self.settings.restart.crash_loop_restarts,
                window = ?self.settings.restart.crash_loop_window,
                "service is crash looping; retrying at the maximum backoff"
            );
        }
    }

    /// Stops and removes a container that should not exist.
    async fn retire(
        &self,
        generation: &ServiceGeneration,
        container: &ContainerInfo,
        why: &str,
        report: &mut ReconcileReport,
    ) {
        let (stack, service, gen_no) = fields(generation);
        tracing::info!(stack, service, generation = gen_no, container = %container.name, why, "removing container");
        if container.state.is_running()
            && let Err(err) = self.runtime.stop(&container.id, DEFAULT_STOP_TIMEOUT).await
            && !is_not_found(&err, ObjectKind::Container)
        {
            self.problem(
                generation,
                format!("cannot stop {}: {err}", container.name),
                report,
            );
        }
        match self.runtime.remove(&container.id).await {
            Ok(()) => report.removed.push(container.name.clone()),
            // Removed meanwhile, e.g. by a deployment's clean-up.
            Err(err) if is_not_found(&err, ObjectKind::Container) => {}
            Err(err) => self.problem(
                generation,
                format!("cannot remove {}: {err}", container.name),
                report,
            ),
        }
    }

    /// Removes the network of a stack that no longer exists.
    async fn collect_network(
        &self,
        stack: &str,
        network: &NetworkSpec,
        report: &mut ReconcileReport,
    ) {
        tracing::info!(stack, network = %network.name, "removing network of a removed stack");
        match self.runtime.remove_network(&network.name).await {
            Ok(()) => report.networks_removed.push(network.name.clone()),
            Err(err) if is_not_found(&err, ObjectKind::Network) => {}
            Err(err) => {
                tracing::warn!(stack, network = %network.name, %err, "cannot remove network");
                report
                    .problems
                    .push(format!("cannot remove network {}: {err}", network.name));
            }
        }
    }

    /// Releases every port reservation no current pointer holds (startup only: a running
    /// deployment reserves ports before it commits them).
    async fn release_stale_ports(&self, report: &mut ReconcileReport) -> Result<()> {
        let mut pointers: BTreeMap<String, Vec<ServicePointer>> = BTreeMap::new();
        for allocation in self.ports.list().await? {
            if self.is_busy(&allocation.stack) {
                continue;
            }
            if !pointers.contains_key(&allocation.stack) {
                let services = self.store.services(&allocation.stack).await?;
                pointers.insert(allocation.stack.clone(), services);
            }
            let held = pointers[&allocation.stack].iter().any(|p| {
                p.service == allocation.service
                    && p.generation == allocation.generation
                    && p.ports.values().any(|&port| port == allocation.port)
            });
            if held {
                continue;
            }
            tracing::info!(
                stack = %allocation.stack, service = %allocation.service,
                generation = allocation.generation, port = allocation.port,
                "releasing port reservation nothing holds"
            );
            match self.ports.release(allocation.port).await {
                Ok(_) => report.ports_released.push(allocation.port),
                Err(err) => report
                    .problems
                    .push(format!("cannot release port {}: {err}", allocation.port)),
            }
        }
        Ok(())
    }

    /// Warns once per pointer that has no container at all: hoster does not recreate it
    /// behind the operator's back; a redeploy does.
    fn warn_missing(
        &self,
        stack: &str,
        pointers: &[ServicePointer],
        containers: &[(ServiceGeneration, ContainerInfo)],
    ) {
        for pointer in pointers {
            let present = containers.iter().any(|(g, _)| {
                g.service.as_ref() == pointer.service && g.generation == pointer.generation
            });
            let key = format!(
                "missing:{stack}/{}/g{}",
                pointer.service, pointer.generation
            );
            if !present && self.reconciler().first_warning(key) {
                tracing::warn!(
                    stack, service = %pointer.service, generation = pointer.generation,
                    "the current generation has no container; redeploy the service to recreate it"
                );
            }
        }
    }

    /// Notes an object that carries `hoster.managed=true` but is not hoster's.
    fn foreign(&self, kind: &str, name: &str, reason: &str, report: &mut ReconcileReport) {
        if self
            .reconciler()
            .first_warning(format!("foreign:{kind}:{name}"))
        {
            tracing::warn!(
                kind,
                name,
                reason,
                "leaving untouched a managed-labelled object that is not hoster's"
            );
        }
        report.foreign.push(name.to_owned());
    }

    fn problem(
        &self,
        generation: &ServiceGeneration,
        message: String,
        report: &mut ReconcileReport,
    ) {
        let (stack, service, gen_no) = fields(generation);
        tracing::warn!(stack, service, generation = gen_no, problem = %message, "reconcile action failed");
        report.problems.push(message);
    }
}

/// The stack a managed network belongs to, read from its `hoster.stack` label and checked
/// against its name; or why it is not hoster's.
fn network_stack(network: &NetworkSpec) -> std::result::Result<Name, String> {
    let stack = network
        .labels
        .get(label::STACK)
        .ok_or_else(|| format!("label {} is missing", label::STACK))?
        .parse::<Name>()
        .map_err(|err| format!("label {}: {err}", label::STACK))?;
    if NetworkSpec::for_stack(&stack).name != network.name {
        return Err("its name does not match its hoster.stack label".to_owned());
    }
    Ok(stack)
}

/// Whether `generation` is the one its service's pointer names.
fn is_current(pointers: &[ServicePointer], generation: &ServiceGeneration) -> bool {
    pointers
        .iter()
        .any(|p| p.service == generation.service.as_ref() && p.generation == generation.generation)
}

/// The log fields of a generation.
fn fields(generation: &ServiceGeneration) -> (&str, &str, u32) {
    (
        generation.stack.as_ref(),
        generation.service.as_ref(),
        generation.generation,
    )
}

fn is_not_found(err: &RuntimeError, kind: ObjectKind) -> bool {
    matches!(err, RuntimeError::NotFound { kind: k, .. } if *k == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RestartPolicy {
        RestartPolicy::default()
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_doubles_from_one_second_to_five_minutes() {
        let mut restarts = Restarts::new(RestartPolicy {
            crash_loop_restarts: usize::MAX,
            ..policy()
        });
        assert!(restarts.is_due(Instant::now()));
        let mut waits = Vec::new();
        for _ in 0..12 {
            let now = Instant::now();
            assert!(restarts.is_due(now));
            restarts.record(now);
            let next = restarts.due_after(now).expect("a wait after a restart");
            waits.push((next - now).as_secs());
            // Just before the wait is over the restart is not due yet.
            tokio::time::advance(next - now - Duration::from_millis(1)).await;
            assert!(!restarts.is_due(Instant::now()));
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300]);
    }

    #[tokio::test(start_paused = true)]
    async fn five_restarts_in_ten_minutes_is_a_crash_loop_at_maximum_backoff() {
        let mut restarts = Restarts::new(policy());
        let mut became = Vec::new();
        for _ in 0..5 {
            let now = Instant::now();
            became.push(restarts.record(now));
            tokio::time::advance(restarts.due_after(now).unwrap() - now).await;
        }
        assert_eq!(became, [false, false, false, false, true]);
        assert!(restarts.is_crash_looping());
        let now = Instant::now();
        assert_eq!(
            restarts.due_after(now).map(|at| at - now),
            None,
            "the crash-loop wait is over"
        );
        restarts.record(now);
        assert_eq!(
            restarts.due_after(now).map(|at| at - now),
            Some(Duration::from_secs(300))
        );
        // Still crashing every five minutes: it stays crash looping.
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(300)).await;
            assert!(!restarts.record(Instant::now()));
            assert!(restarts.is_crash_looping());
        }
        // Up for a whole window without a restart: the history resets.
        tokio::time::advance(Duration::from_secs(5 * 60)).await;
        assert!(
            !restarts.running(Instant::now()),
            "the last restart is 5 min ago"
        );
        tokio::time::advance(Duration::from_secs(5 * 60)).await;
        assert!(restarts.running(Instant::now()));
        assert!(!restarts.is_crash_looping());
        let now = Instant::now();
        restarts.record(now);
        assert_eq!(
            restarts.due_after(now).map(|at| at - now),
            Some(Duration::from_secs(1))
        );
        assert_eq!(restarts.total, 11);
    }

    #[tokio::test(start_paused = true)]
    async fn restarts_spread_beyond_the_window_never_crash_loop() {
        let mut restarts = Restarts::new(policy());
        for _ in 0..10 {
            restarts.record(Instant::now());
            tokio::time::advance(Duration::from_secs(3 * 60)).await;
        }
        assert!(!restarts.is_crash_looping());
    }
}
