//! Resource usage (ADR-0066): the background sampler and the in-memory cache pages read.
//!
//! Every [`ResourceSampler::INTERVAL`] the sampler lists the managed containers, reads the
//! stats of the running ones (at most [`ResourceSampler::CONCURRENCY`] at a time, all within
//! [`ResourceSampler::DEADLINE`]) and reads the host's CPU times, memory and the filesystem of
//! podman's storage root through [`Machine`]. It turns them into samples, CPU as a rate between
//! two readings, and records them in [`ResourceUsage`]: the latest sample and a ring of
//! [`ResourceUsage::HISTORY`] per listed container and for the host. Pages read only that
//! cache, never podman or `/proc`, and can await [`ResourceUsage::changes`] to learn that a
//! tick landed. Nothing is persisted.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::machine::{CpuTimes, FilesystemUsage, Machine, MemoryUsage};
use crate::runtime::{ContainerId, ContainerInfo, ContainerRuntime, label};
use crate::store::Timestamp;

/// Bytes in use out of a total: the host's memory or a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub used: u64,
    pub total: u64,
}

/// `MemTotal − MemAvailable` out of `MemTotal` (ADR-0066 §5).
impl From<MemoryUsage> for Usage {
    fn from(memory: MemoryUsage) -> Self {
        Self {
            used: memory.used(),
            total: memory.total,
        }
    }
}

/// Size minus what an unprivileged user may still write, out of the size (ADR-0066 §5).
impl From<FilesystemUsage> for Usage {
    fn from(fs: FilesystemUsage) -> Self {
        Self {
            used: fs.total.saturating_sub(fs.available),
            total: fs.total,
        }
    }
}

/// One tick's host figures; each is `None` when unknown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostSample {
    pub at: Timestamp,
    /// The busy share of all cores since the previous tick, in percent (100 = every core).
    pub cpu: Option<f64>,
    pub memory: Option<Usage>,
    /// The filesystem of podman's storage root.
    pub disk: Option<Usage>,
}

/// One tick's figures of a container; each is `None` when unknown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContainerSample {
    pub at: Timestamp,
    /// CPU since the container's previous reading, in percent of one core (150 = 1.5 cores).
    pub cpu: Option<f64>,
    /// Podman's memory `usage`, in bytes (ADR-0066 §3).
    pub memory: Option<u64>,
}

impl ContainerSample {
    /// A sample with no figures: the container is not running or did not answer.
    fn unknown(at: Timestamp) -> Self {
        Self {
            at,
            cpu: None,
            memory: None,
        }
    }
}

/// A sample of one container, with who it belongs to, as one tick records it.
#[derive(Debug, Clone, PartialEq)]
pub struct ContainerReading {
    pub id: ContainerId,
    /// The `hoster.stack` label; empty when missing.
    pub stack: String,
    /// The `hoster.service` label; empty when missing.
    pub service: String,
    pub sample: ContainerSample,
}

/// `sample`, of the container `info` lists.
impl From<(&ContainerInfo, ContainerSample)> for ContainerReading {
    fn from((info, sample): (&ContainerInfo, ContainerSample)) -> Self {
        let label = |name: &str| info.labels.get(name).cloned().unwrap_or_default();
        Self {
            id: info.id.clone(),
            stack: label(label::STACK),
            service: label(label::SERVICE),
            sample,
        }
    }
}

/// Samples, oldest first, at most [`ResourceUsage::HISTORY`] of them.
#[derive(Debug, Clone, PartialEq)]
pub struct History<S> {
    samples: Vec<S>,
}

impl<S> History<S> {
    /// The newest sample; `None` before the first tick.
    pub fn latest(&self) -> Option<&S> {
        self.samples.last()
    }

    /// Every sample, oldest first.
    pub fn samples(&self) -> &[S] {
        &self.samples
    }
}

/// What the sampler holds about one container.
#[derive(Debug, Clone, PartialEq)]
#[allow(
    dead_code,
    reason = "read by the stack overview's usage columns (task rap)"
)]
pub struct ContainerUsage {
    pub stack: String,
    pub service: String,
    pub history: History<ContainerSample>,
}

/// The sampler's cache: a cheap handle, clones share it. Empty (every figure unknown) until
/// the first tick, and so it stays for a handle no sampler records into.
#[derive(Debug, Clone, Default)]
pub struct ResourceUsage {
    inner: Arc<Shared>,
}

#[derive(Debug, Default)]
struct Shared {
    state: RwLock<State>,
    /// Bumped after every tick.
    ticks: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct State {
    host: Ring<HostSample>,
    containers: BTreeMap<ContainerId, Tracked>,
}

#[derive(Debug)]
struct Tracked {
    stack: String,
    service: String,
    samples: Ring<ContainerSample>,
}

/// A ring of the last [`ResourceUsage::HISTORY`] samples.
#[derive(Debug)]
struct Ring<S>(VecDeque<S>);

impl<S> Default for Ring<S> {
    fn default() -> Self {
        Self(VecDeque::with_capacity(ResourceUsage::HISTORY))
    }
}

impl<S: Clone> Ring<S> {
    fn push(&mut self, sample: S) {
        if self.0.len() == ResourceUsage::HISTORY {
            self.0.pop_front();
        }
        self.0.push_back(sample);
    }

    fn history(&self) -> History<S> {
        History {
            samples: self.0.iter().cloned().collect(),
        }
    }
}

impl ResourceUsage {
    /// How many samples a ring keeps: an hour at [`ResourceSampler::INTERVAL`] (ADR-0066 §6).
    pub const HISTORY: usize = 360;

    /// The host's latest sample and history.
    pub fn host(&self) -> History<HostSample> {
        self.read().host.history()
    }

    /// The latest sample and history of every listed container of `stack`, by container id.
    #[allow(
        dead_code,
        reason = "read by the stack overview's usage columns (task rap)"
    )]
    pub fn stack(&self, stack: &str) -> BTreeMap<ContainerId, ContainerUsage> {
        self.read()
            .containers
            .iter()
            .filter(|(_, tracked)| tracked.stack == stack)
            .map(|(id, tracked)| {
                let usage = ContainerUsage {
                    stack: tracked.stack.clone(),
                    service: tracked.service.clone(),
                    history: tracked.samples.history(),
                };
                (id.clone(), usage)
            })
            .collect()
    }

    /// A receiver that changes after every tick (a tick counter), for live pages to await.
    #[allow(
        dead_code,
        reason = "awaited by the stack overview's live stream (task rap)"
    )]
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.inner.ticks.subscribe()
    }

    /// Records one tick: `host`, and `containers` as listed, or `None` when the runtime could
    /// not list them. A listed container's sample goes into its ring (a new one for a new id),
    /// and a ring whose container is not listed is dropped. Without a list, every ring gets an
    /// unknown sample at the host's time. Then [`Self::changes`] fires.
    pub(crate) fn record(&self, host: HostSample, containers: Option<Vec<ContainerReading>>) {
        {
            let mut state = self
                .inner
                .state
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            state.host.push(host);
            match containers {
                Some(listed) => {
                    let mut kept = BTreeMap::new();
                    for reading in listed {
                        let mut tracked =
                            state
                                .containers
                                .remove(&reading.id)
                                .unwrap_or_else(|| Tracked {
                                    stack: reading.stack,
                                    service: reading.service,
                                    samples: Ring::default(),
                                });
                        tracked.samples.push(reading.sample);
                        kept.insert(reading.id, tracked);
                    }
                    state.containers = kept;
                }
                None => {
                    for tracked in state.containers.values_mut() {
                        tracked.samples.push(ContainerSample::unknown(host.at));
                    }
                }
            }
        }
        self.inner.ticks.send_modify(|ticks| *ticks += 1);
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.inner
            .state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A container's raw counters, and when on the sampler's monotonic clock they arrived.
#[derive(Debug, Clone, Copy)]
struct Reading {
    at: Instant,
    cpu_time: Duration,
    memory: u64,
}

impl Reading {
    /// The CPU used between `earlier` and this reading, in percent of one core; `None` when no
    /// time passed or the counter went backwards (a restart gives a new cgroup).
    fn cpu_percent(&self, earlier: &Reading) -> Option<f64> {
        let used = self.cpu_time.checked_sub(earlier.cpu_time)?;
        let wall = self.at.checked_duration_since(earlier.at)?;
        (!wall.is_zero()).then(|| used.as_secs_f64() / wall.as_secs_f64() * 100.0)
    }
}

/// What one tick learned from the runtime about the containers.
#[derive(Debug, Default)]
struct Answers {
    /// The listed containers; `None` when listing failed.
    listed: Option<Vec<ContainerInfo>>,
    /// The stats calls that answered: `None` for a container that is not running (or gone).
    /// A running container missing here failed or did not answer in time.
    stats: HashMap<ContainerId, Option<Reading>>,
    /// The first failure, if any.
    error: Option<String>,
}

/// Logs a source's failures once per change of state (ADR-0066 §7).
#[derive(Debug, Default)]
struct Outage {
    failing: bool,
}

impl Outage {
    fn report(&mut self, source: &str, error: Option<&str>) {
        match (self.failing, error) {
            (false, Some(error)) => tracing::warn!(
                error,
                "cannot read {source}; resource usage shows it as unknown until it answers"
            ),
            (true, None) => tracing::info!("reading {source} again"),
            _ => {}
        }
        self.failing = error.is_some();
    }
}

/// The background task of ADR-0066 that fills a [`ResourceUsage`].
pub struct ResourceSampler<R> {
    runtime: Arc<R>,
    machine: Machine,
    usage: ResourceUsage,
    /// Podman's storage root, read from the engine info until it is known.
    storage_root: Option<PathBuf>,
    host_cpu: Option<CpuTimes>,
    /// Each running container's latest reading, the base of its next CPU figure.
    readings: HashMap<ContainerId, Reading>,
    runtime_outage: Outage,
    engine_outage: Outage,
}

impl<R: ContainerRuntime> ResourceSampler<R> {
    /// How often it samples (ADR-0066 §1).
    pub const INTERVAL: Duration = Duration::from_secs(10);
    /// How long a tick may wait for the runtime, half the interval (ADR-0066 §2).
    pub const DEADLINE: Duration = Duration::from_secs(5);
    /// How many stats calls run at once (ADR-0066 §2).
    pub const CONCURRENCY: usize = 8;

    /// A sampler of `runtime`'s managed containers and of `machine`, into an empty cache.
    pub fn new(runtime: Arc<R>, machine: Machine) -> Self {
        Self {
            runtime,
            machine,
            usage: ResourceUsage::default(),
            storage_root: None,
            host_cpu: None,
            readings: HashMap::new(),
            runtime_outage: Outage::default(),
            engine_outage: Outage::default(),
        }
    }

    /// The cache this sampler records into, for the pages.
    pub fn usage(&self) -> ResourceUsage {
        self.usage.clone()
    }

    /// Samples at once and then every [`Self::INTERVAL`] until `cancel` fires. A tick that
    /// overruns skips the ticks it missed instead of queueing them.
    pub async fn run(mut self, cancel: CancellationToken) {
        let mut ticker = tokio::time::interval(Self::INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                () = self.tick() => {}
            }
        }
        tracing::debug!("resource sampler stopped");
    }

    /// One sample of every listed container and of the host.
    async fn tick(&mut self) {
        let at = Timestamp::now();
        let deadline = Instant::now() + Self::DEADLINE;
        if self.storage_root.is_none() {
            self.read_storage_root(deadline).await;
        }
        let answers = self.ask_runtime(deadline).await;
        self.runtime_outage
            .report("container stats from podman", answers.error.as_deref());
        let containers = self.container_readings(at, answers);
        let host = self.host_sample(at);
        self.usage.record(host, containers);
    }

    async fn read_storage_root(&mut self, deadline: Instant) {
        let error = match timeout_at(deadline, self.runtime.engine()).await {
            Ok(Ok(engine)) => {
                self.storage_root = engine.storage_root;
                None
            }
            Ok(Err(err)) => Some(format!("{:#}", anyhow::Error::from(err))),
            Err(_) => Some(format!("no answer within {:?}", Self::DEADLINE)),
        };
        self.engine_outage
            .report("podman's storage root", error.as_deref());
    }

    /// Lists the managed containers and reads the stats of the running ones, until `deadline`.
    async fn ask_runtime(&self, deadline: Instant) -> Answers {
        let late = || format!("podman did not answer within {:?}", Self::DEADLINE);
        let listed = match timeout_at(deadline, self.runtime.list_managed()).await {
            Ok(Ok(listed)) => listed,
            Ok(Err(err)) => {
                return Answers {
                    error: Some(format!("{:#}", anyhow::Error::from(err))),
                    ..Answers::default()
                };
            }
            Err(_) => {
                return Answers {
                    error: Some(late()),
                    ..Answers::default()
                };
            }
        };
        let running: Vec<ContainerId> = listed
            .iter()
            .filter(|info| info.state.is_running())
            .map(|info| info.id.clone())
            .collect();
        let runtime = &self.runtime;
        let mut calls = stream::iter(running)
            .map(|id| async move {
                let stats = runtime.stats(&id).await;
                (id, stats, Instant::now())
            })
            .buffer_unordered(Self::CONCURRENCY);
        let mut answers = Answers::default();
        loop {
            match timeout_at(deadline, calls.next()).await {
                Ok(Some((id, Ok(stats), at))) => {
                    let reading = stats.map(|stats| Reading {
                        at,
                        cpu_time: stats.cpu_time,
                        memory: stats.memory,
                    });
                    answers.stats.insert(id, reading);
                }
                Ok(Some((_, Err(err), _))) => {
                    answers
                        .error
                        .get_or_insert_with(|| format!("{:#}", anyhow::Error::from(err)));
                }
                Ok(None) => break,
                Err(_) => {
                    answers.error.get_or_insert_with(late);
                    break;
                }
            }
        }
        answers.listed = Some(listed);
        answers
    }

    /// The samples of the listed containers, updating the readings CPU is computed from;
    /// `None` when the containers could not be listed.
    fn container_readings(
        &mut self,
        at: Timestamp,
        mut answers: Answers,
    ) -> Option<Vec<ContainerReading>> {
        let listed = answers.listed.take()?;
        self.readings
            .retain(|id, _| listed.iter().any(|info| info.id == *id));
        let readings = listed
            .iter()
            .map(|info| {
                let sample = match answers.stats.remove(&info.id) {
                    Some(Some(reading)) => {
                        let cpu = self
                            .readings
                            .insert(info.id.clone(), reading)
                            .and_then(|earlier| reading.cpu_percent(&earlier));
                        ContainerSample {
                            at,
                            cpu,
                            memory: Some(reading.memory),
                        }
                    }
                    // Not running: its next reading starts over.
                    Some(None) => {
                        self.readings.remove(&info.id);
                        ContainerSample::unknown(at)
                    }
                    // Not running as listed, or no answer: the earlier reading stays the
                    // base while it runs.
                    None => {
                        if !info.state.is_running() {
                            self.readings.remove(&info.id);
                        }
                        ContainerSample::unknown(at)
                    }
                };
                ContainerReading::from((info, sample))
            })
            .collect();
        Some(readings)
    }

    fn host_sample(&mut self, at: Timestamp) -> HostSample {
        let cpu_times = self.machine.cpu_times();
        let cpu = cpu_times
            .zip(self.host_cpu)
            .and_then(|(now, earlier)| now.busy_fraction(&earlier))
            .map(|busy| busy * 100.0);
        if cpu_times.is_some() {
            self.host_cpu = cpu_times;
        }
        HostSample {
            at,
            cpu,
            memory: self.machine.memory_usage().map(Usage::from),
            disk: self
                .storage_root
                .as_deref()
                .and_then(|root| self.machine.filesystem(root))
                .map(Usage::from),
        }
    }
}

#[cfg(test)]
mod tests;
