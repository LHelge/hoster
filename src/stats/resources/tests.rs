use std::path::Path;

use tempfile::TempDir;

use super::*;
use crate::runtime::{
    ContainerSpec, ContainerState, ContainerStats, FakeRuntime, NetworkSpec, Operation,
    RuntimeError, ServiceGeneration,
};
use crate::store::DeploymentId;

const SECOND: Duration = Duration::from_secs(1);
const GIB: u64 = 1 << 30;

/// A sampler over a [`FakeRuntime`] and a machine whose `/proc` is in a temporary directory,
/// ticked by hand on tokio's paused clock.
struct Harness {
    root: TempDir,
    runtime: Arc<FakeRuntime>,
    sampler: ResourceSampler<FakeRuntime>,
    usage: ResourceUsage,
}

impl Harness {
    async fn new() -> Self {
        let root = TempDir::new().unwrap();
        std::fs::create_dir(root.path().join("proc")).unwrap();
        let runtime = Arc::new(FakeRuntime::new());
        let sampler = ResourceSampler::new(Arc::clone(&runtime), Machine::under(root.path(), 1000));
        let usage = sampler.usage();
        let h = Self {
            root,
            runtime,
            sampler,
            usage,
        };
        h.host(100, 400);
        h.memory(16 * GIB, 12 * GIB);
        h
    }

    /// `/proc/stat` with `busy` and `idle` jiffies in total.
    fn host(&self, busy: u64, idle: u64) {
        let stat =
            format!("cpu  {busy} 0 0 {idle} 0 0 0 0 0 0\ncpu0 {busy} 0 0 {idle} 0 0 0 0 0 0\n");
        std::fs::write(self.root.path().join("proc/stat"), stat).unwrap();
    }

    /// `/proc/meminfo` with `total` and `available` bytes.
    fn memory(&self, total: u64, available: u64) {
        let meminfo = format!(
            "MemTotal: {} kB\nMemFree: 1 kB\nMemAvailable: {} kB\n",
            total / 1024,
            available / 1024
        );
        std::fs::write(self.root.path().join("proc/meminfo"), meminfo).unwrap();
    }

    /// Starts a container of service `service` in stack `stack`.
    async fn start(&self, stack: &str, service: &str, generation: u32) -> ContainerId {
        let generation = ServiceGeneration {
            stack: stack.parse().unwrap(),
            service: service.parse().unwrap(),
            generation,
        };
        let image = "docker.io/library/app:1"
            .parse::<crate::stack::ImageRef>()
            .unwrap()
            .pin(format!("sha256:{}", "cd".repeat(32)).parse().unwrap());
        let spec = ContainerSpec::new(&generation, DeploymentId::from(1), image.clone());
        self.runtime
            .ensure_network(&NetworkSpec::for_stack(&generation.stack))
            .await
            .unwrap();
        self.runtime.pull_image(&image).await.unwrap();
        let id = self.runtime.create_container(&spec).await.unwrap();
        self.runtime.start(&id).await.unwrap();
        id
    }

    fn counters(&self, id: &ContainerId, cpu_ms: u64, memory: u64) {
        self.runtime.set_stats(
            id,
            ContainerStats {
                cpu_time: Duration::from_millis(cpu_ms),
                memory,
            },
        );
    }

    /// Lets an interval pass, then ticks.
    async fn tick(&mut self) {
        tokio::time::advance(ResourceSampler::<FakeRuntime>::INTERVAL).await;
        self.sampler.tick().await;
    }

    /// The latest sample of container `id` of stack `stack`.
    fn latest(&self, stack: &str, id: &ContainerId) -> ContainerSample {
        *self.usage.stack(stack)[id].history.latest().unwrap()
    }

    fn host_latest(&self) -> HostSample {
        *self.usage.host().latest().unwrap()
    }

    fn calls(&self, operation: Operation) -> usize {
        self.runtime
            .calls()
            .into_iter()
            .filter(|call| *call == operation)
            .count()
    }
}

fn boom(operation: Operation) -> RuntimeError {
    RuntimeError::Unsupported { operation }
}

#[test]
fn every_figure_is_unknown_before_the_first_tick() {
    let usage = ResourceUsage::default();
    assert_eq!(usage.host().latest(), None);
    assert!(usage.host().samples().is_empty());
    assert!(usage.stack("site").is_empty());
}

#[tokio::test(start_paused = true)]
async fn cpu_is_the_rate_between_two_readings_and_unknown_at_first() {
    let mut h = Harness::new().await;
    let web = h.start("site", "web", 1).await;
    h.counters(&web, 1_000, 64 << 20);
    h.tick().await;
    let first = h.latest("site", &web);
    assert_eq!(first.cpu, None, "no earlier reading");
    assert_eq!(first.memory, Some(64 << 20));
    let host = h.host_latest();
    assert_eq!(host.cpu, None, "the host's first tick has no CPU figure");
    assert_eq!(
        host.memory,
        Some(Usage {
            used: 4 * GIB,
            total: 16 * GIB
        })
    );

    // 15 s of CPU in 10 s of wall time is 1.5 cores; the host was busy 300 of 400 jiffies.
    h.counters(&web, 16_000, 80 << 20);
    h.host(400, 500);
    h.tick().await;
    let second = h.latest("site", &web);
    assert_eq!(second.cpu, Some(150.0));
    assert_eq!(second.memory, Some(80 << 20));
    assert_eq!(h.host_latest().cpu, Some(75.0));

    let usage = &h.usage.stack("site")[&web];
    assert_eq!(
        (
            usage.stack.as_str(),
            usage.service.as_str(),
            usage.generation
        ),
        ("site", "web", Some(1))
    );
    assert_eq!(usage.history.samples().len(), 2);
    assert!(usage.history.samples()[0].at <= usage.history.samples()[1].at);
}

#[tokio::test(start_paused = true)]
async fn a_counter_going_backwards_has_no_cpu_figure() {
    let mut h = Harness::new().await;
    let web = h.start("site", "web", 1).await;
    h.counters(&web, 5_000, 1);
    h.tick().await;
    h.counters(&web, 1_000, 1);
    h.tick().await;
    assert_eq!(h.latest("site", &web).cpu, None);
    // The reading that went backwards is the base of the next one.
    h.counters(&web, 2_000, 1);
    h.tick().await;
    assert_eq!(h.latest("site", &web).cpu, Some(10.0));
}

#[tokio::test(start_paused = true)]
async fn a_ring_keeps_an_hour_of_samples() {
    let mut h = Harness::new().await;
    let web = h.start("site", "web", 1).await;
    for _ in 0..=ResourceUsage::HISTORY {
        h.tick().await;
    }
    assert_eq!(
        h.usage.stack("site")[&web].history.samples().len(),
        ResourceUsage::HISTORY
    );
    assert_eq!(h.usage.host().samples().len(), ResourceUsage::HISTORY);
}

#[tokio::test(start_paused = true)]
async fn containers_no_longer_listed_are_dropped_and_stacks_are_kept_apart() {
    let mut h = Harness::new().await;
    let old = h.start("site", "web", 1).await;
    let db = h.start("shop", "db", 1).await;
    h.tick().await;
    assert_eq!(h.usage.stack("site").len(), 1);
    assert_eq!(h.usage.stack("shop").len(), 1);

    // A deploy: the new generation starts with an empty ring, the old one goes away.
    let new = h.start("site", "web", 2).await;
    h.runtime.remove(&old).await.unwrap();
    h.tick().await;
    let site = h.usage.stack("site");
    assert_eq!(site.keys().collect::<Vec<_>>(), [&new]);
    assert_eq!(site[&new].history.samples().len(), 1);
    assert_eq!(h.usage.stack("shop")[&db].history.samples().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn stopped_containers_are_not_read_and_get_unknown_samples() {
    let mut h = Harness::new().await;
    let web = h.start("site", "web", 1).await;
    h.counters(&web, 1_000, 1);
    h.tick().await;
    assert_eq!(h.calls(Operation::Stats), 1);

    h.runtime.set_state(&web, ContainerState::Exited(Some(0)));
    h.tick().await;
    assert_eq!(h.calls(Operation::Stats), 1, "no stats call for it");
    assert_eq!(
        h.latest("site", &web),
        ContainerSample::unknown(h.latest("site", &web).at)
    );
    assert_eq!(h.usage.stack("site")[&web].history.samples().len(), 2);

    // Started again: its first reading has no CPU figure.
    h.runtime.start(&web).await.unwrap();
    h.counters(&web, 9_000, 1);
    h.tick().await;
    assert_eq!(h.latest("site", &web).cpu, None);
    assert_eq!(h.latest("site", &web).memory, Some(1));
}

#[tokio::test(start_paused = true)]
async fn a_failing_runtime_keeps_the_host_figures_and_marks_containers_unknown() {
    let mut h = Harness::new().await;
    let web = h.start("site", "web", 1).await;
    h.counters(&web, 1_000, 1);
    h.tick().await;

    h.runtime
        .fail_always(Operation::ListManaged, boom(Operation::ListManaged));
    h.tick().await;
    h.host(200, 500);
    h.tick().await;
    let sample = h.latest("site", &web);
    assert_eq!((sample.cpu, sample.memory), (None, None));
    assert_eq!(
        h.usage.stack("site")[&web].history.samples().len(),
        3,
        "kept"
    );
    let host = h.host_latest();
    assert!(host.cpu.is_some() && host.memory.is_some(), "{host:?}");
    assert!(h.sampler.runtime_outage.failing);

    // Recovered: the reading before the outage is still the base.
    h.runtime.clear_failures();
    h.counters(&web, 4_000, 1);
    h.tick().await;
    assert_eq!(h.latest("site", &web).cpu, Some(10.0));
    assert!(!h.sampler.runtime_outage.failing);

    // A failing stats call leaves that container unknown.
    h.runtime
        .fail_next(Operation::Stats, boom(Operation::Stats));
    h.tick().await;
    assert_eq!(h.latest("site", &web).memory, None);
    assert!(h.sampler.runtime_outage.failing);
}

#[tokio::test(start_paused = true)]
async fn disk_is_the_storage_root_filesystem_read_again_while_unknown() {
    let mut h = Harness::new().await;
    let storage = TempDir::new().unwrap();
    h.runtime.set_storage_root(storage.path());
    h.runtime
        .fail_next(Operation::Engine, boom(Operation::Engine));
    h.tick().await;
    assert_eq!(h.host_latest().disk, None);
    assert!(h.sampler.engine_outage.failing);

    h.tick().await;
    let disk = h.host_latest().disk.expect("the storage root's filesystem");
    let fs = h
        .sampler
        .machine
        .filesystem(Path::new(storage.path()))
        .unwrap();
    assert_eq!(disk.total, fs.total);
    assert!(disk.used <= disk.total);
    h.tick().await;
    assert_eq!(h.calls(Operation::Engine), 2, "not read again once known");
    assert!(!h.sampler.engine_outage.failing);
}

#[tokio::test(start_paused = true)]
async fn each_tick_bumps_the_change_signal() {
    let mut h = Harness::new().await;
    let mut changes = h.usage.changes();
    assert!(!changes.has_changed().unwrap());
    h.tick().await;
    assert!(changes.has_changed().unwrap());
    assert_eq!(*changes.borrow_and_update(), 1);
    h.tick().await;
    assert_eq!(*changes.borrow_and_update(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_task_samples_every_interval_until_cancelled() {
    let h = Harness::new().await;
    let mut changes = h.usage.changes();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(h.sampler.run(cancel.clone()));
    // The first tick runs at once, the next one an interval later.
    changes.changed().await.unwrap();
    let start = Instant::now();
    changes.changed().await.unwrap();
    assert_eq!(start.elapsed(), ResourceSampler::<FakeRuntime>::INTERVAL);
    assert_eq!(h.usage.host().samples().len(), 2);
    cancel.cancel();
    tokio::time::timeout(SECOND, task)
        .await
        .expect("cancel stops the sampler")
        .unwrap();
}

#[test]
fn recorded_samples_reach_the_readers() {
    let usage = ResourceUsage::default();
    let at = Timestamp::now();
    let id = ContainerId::from("abc".to_owned());
    let reading = ContainerReading {
        id: id.clone(),
        stack: "site".into(),
        service: "web".into(),
        generation: Some(3),
        sample: ContainerSample {
            at,
            cpu: Some(12.5),
            memory: Some(1 << 20),
        },
    };
    let host = HostSample {
        at,
        cpu: Some(3.0),
        memory: None,
        disk: None,
    };
    usage.record(host, Some(vec![reading.clone()]));
    usage.record(host, None);
    let site = usage.stack("site");
    assert_eq!(
        site[&id].history.samples(),
        [reading.sample, ContainerSample::unknown(at)]
    );
    assert_eq!(usage.host().latest(), Some(&host));
}
