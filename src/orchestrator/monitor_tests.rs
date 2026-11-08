//! Health monitor scenarios (ADR-0038): the orchestrator against [`FakeRuntime`], a real
//! (temporary) store and a fake prober, with passes run at chosen instants.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::tests::{Harness, harness};
use super::*;
use crate::config::PortRange;
use crate::health::Check;
use crate::runtime::{ContainerState, FakeRuntime, Operation, RuntimeError};

use HealthStatus as H;

/// Answers probes by container port: a scripted result, `HTTP 200` otherwise. Counts probes.
#[derive(Default)]
struct FakeProber {
    answers: StdMutex<BTreeMap<u16, Result<String, String>>>,
    probes: StdMutex<Vec<String>>,
}

impl FakeProber {
    fn answer(&self, port: u16, result: Result<&str, &str>) {
        let result = result.map(str::to_owned).map_err(str::to_owned);
        self.answers.lock().unwrap().insert(port, result);
    }

    fn take_probes(&self) -> Vec<String> {
        let mut probes = std::mem::take(&mut *self.probes.lock().unwrap());
        probes.sort();
        probes
    }
}

#[async_trait]
impl Prober for FakeProber {
    async fn probe(&self, check: &Check) -> std::result::Result<String, String> {
        let text = check.to_string();
        self.probes.lock().unwrap().push(text.clone());
        self.answers
            .lock()
            .unwrap()
            .iter()
            .find(|(port, _)| text.contains(&format!("container port {port} ")))
            .map_or_else(|| Ok("HTTP 200".to_owned()), |(_, r)| r.clone())
    }
}

/// NOTES live (api g2, web g2, storage g1), watched by a monitor probing through a fake.
async fn monitored() -> (Harness, Arc<Orchestrator<FakeRuntime>>, Arc<FakeProber>) {
    let h = harness().await;
    h.bootstrap().await;
    let prober = Arc::new(FakeProber::default());
    let orchestrator = Orchestrator::with_prober(
        h.store.clone(),
        Arc::clone(&h.runtime),
        Arc::clone(&h.resolver) as Arc<dyn TagResolver>,
        PortAllocator::new(h.store.clone(), PortRange::try_from(20000..=20015).unwrap()),
        h.secrets.clone(),
        Settings {
            settle: Duration::from_millis(20),
            settle_poll: Duration::from_millis(5),
            ..Settings::default()
        },
        Arc::clone(&prober) as Arc<dyn Prober>,
    );
    (h, orchestrator, prober)
}

/// `(service, from, to)` of health events.
fn changes(events: &[UiEvent]) -> Vec<(&str, H, H)> {
    let mut changes: Vec<_> = events
        .iter()
        .map(|e| match e {
            UiEvent::Health {
                service, from, to, ..
            } => (service.as_str(), *from, *to),
        })
        .collect();
    changes.sort_by_key(|c| c.0);
    changes
}

async fn health(orchestrator: &Orchestrator<FakeRuntime>, service: &str) -> HealthStats {
    orchestrator.status().await.unwrap()[0]
        .services
        .iter()
        .find(|s| s.name == service)
        .unwrap()
        .health
        .clone()
}

#[tokio::test]
async fn live_services_are_probed_periodically_and_changes_reach_ui_events() {
    let (_h, orchestrator, prober) = monitored().await;
    let mut events = orchestrator.subscribe();
    let start = Instant::now();
    let at = |secs: u64| start + Duration::from_secs(secs);

    // First pass: every live service is probed with its default check and becomes healthy.
    let first = orchestrator.monitor_at(at(0)).await.unwrap();
    assert_eq!(
        changes(&first),
        [
            ("api", H::Unknown, H::Healthy),
            ("storage", H::Unknown, H::Healthy),
            ("web", H::Unknown, H::Healthy),
        ]
    );
    let probes = prober.take_probes();
    assert_eq!(probes.len(), 3);
    assert!(
        probes[0].starts_with("GET /healthz on container port 80 "),
        "{probes:?}"
    );
    for event in &first {
        assert_eq!(&events.try_recv().unwrap(), event);
    }

    // Nothing is due before the 15 s interval is over.
    assert!(orchestrator.monitor_at(at(5)).await.unwrap().is_empty());
    assert!(prober.take_probes().is_empty());

    // api starts failing: unhealthy at the next probe, one event, nothing else changes.
    prober.answer(8080, Err("HTTP 503, expected 2xx-3xx"));
    let failing = orchestrator.monitor_at(at(15)).await.unwrap();
    assert_eq!(changes(&failing), [("api", H::Healthy, H::Unhealthy)]);
    assert_eq!(prober.take_probes().len(), 3);
    assert!(matches!(
        events.try_recv().unwrap(),
        UiEvent::Health { ref service, to: H::Unhealthy, ref detail, .. }
            if service == "api" && detail == "HTTP 503, expected 2xx-3xx"
    ));
    assert_eq!(events.try_recv().unwrap_err(), TryRecvError::Empty);

    // A steady failure is not reported again; the statistics count it.
    assert!(orchestrator.monitor_at(at(30)).await.unwrap().is_empty());
    let stats = health(&orchestrator, "api").await;
    assert_eq!(stats.status, H::Unhealthy);
    assert_eq!(stats.since, Some(at(15)));
    assert_eq!((stats.probes, stats.failures, stats.changes), (3, 2, 2));

    // It recovers (`healthy_after` defaults to 1).
    prober.answer(8080, Ok("HTTP 200"));
    let recovered = orchestrator.monitor_at(at(45)).await.unwrap();
    assert_eq!(changes(&recovered), [("api", H::Unhealthy, H::Healthy)]);
    assert_eq!(health(&orchestrator, "web").await.status, H::Healthy);
}

#[tokio::test]
async fn an_exited_container_is_unhealthy_without_a_probe() {
    let (h, orchestrator, prober) = monitored().await;
    let start = Instant::now();
    orchestrator.monitor_at(start).await.unwrap();
    prober.take_probes();

    let web = h.container("hoster-notes-web-g2").id;
    h.runtime.set_state(&web, ContainerState::Exited(Some(1)));
    let events = orchestrator
        .monitor_at(start + Duration::from_secs(15))
        .await
        .unwrap();
    assert_eq!(changes(&events), [("web", H::Healthy, H::Unhealthy)]);
    let UiEvent::Health { detail, .. } = &events[0];
    assert!(detail.starts_with("the container is exited"), "{detail}");
    assert_eq!(prober.take_probes().len(), 2);
    // The monitor never restarts anything (ADR-0038); that is the reconciler's job.
    assert_eq!(
        h.container("hoster-notes-web-g2").state,
        ContainerState::Exited(Some(1))
    );
}

#[tokio::test]
async fn the_reconcilers_crash_loop_verdict_is_a_status() {
    let (h, orchestrator, _prober) = monitored().await;
    let mut events = orchestrator.subscribe();
    let start = Instant::now();
    orchestrator.monitor_at(start).await.unwrap();
    while events.try_recv().is_ok() {}

    // Five restarts in a row of the backoff (0, 1, 3, 7, 15 s): crash looping (ADR-0028).
    let api = h.container("hoster-notes-api-g2").id;
    for t in [0.0, 1.0, 3.0, 7.0, 15.0] {
        h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
        let report = orchestrator
            .reconcile_at(start + Duration::from_secs_f64(t))
            .await
            .unwrap();
        assert_eq!(report.started, ["hoster-notes-api-g2"], "at {t}");
    }
    let looping = orchestrator
        .monitor_at(start + Duration::from_secs(16))
        .await
        .unwrap();
    assert_eq!(changes(&looping), [("api", H::Healthy, H::CrashLooping)]);
    assert!(matches!(
        events.try_recv().unwrap(),
        UiEvent::Health {
            to: H::CrashLooping,
            ..
        }
    ));
    let stats = health(&orchestrator, "api").await;
    assert_eq!(stats.status, H::CrashLooping);
    assert!(stats.last.unwrap().contains("restarted it 5 times"));
}

#[tokio::test]
async fn a_new_generation_is_watched_and_the_old_one_forgotten() {
    let (h, orchestrator, prober) = monitored().await;
    let start = Instant::now();
    orchestrator.monitor_at(start).await.unwrap();
    prober.answer(8080, Err("cannot connect: connection refused"));
    orchestrator
        .monitor_at(start + Duration::from_secs(15))
        .await
        .unwrap();
    assert_eq!(health(&orchestrator, "api").await.status, H::Unhealthy);

    // A deployment replaces api; the new generation starts out unknown, then is probed.
    let deployed = h.deploy(&[("api", 2)]).await;
    h.finish(&deployed).await;
    assert_eq!(health(&orchestrator, "api").await, HealthStats::default());
    prober.answer(8080, Ok("HTTP 204"));
    let events = orchestrator
        .monitor_at(start + Duration::from_secs(16))
        .await
        .unwrap();
    assert!(matches!(
        &events[..],
        [UiEvent::Health { service, generation: 3, from: H::Unknown, to: H::Healthy, .. }]
            if service == "api"
    ));
    let watched: Vec<_> = orchestrator
        .watchlist()
        .keys()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        watched,
        [
            "hoster-notes-api-g3",
            "hoster-notes-storage-g1",
            "hoster-notes-web-g2"
        ]
    );
}

/// The next UI event, within 5 s.
async fn next(events: &mut broadcast::Receiver<UiEvent>) -> UiEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("an event within 5 s")
        .unwrap()
}

/// The loop on real time (the store's pool does not work with paused time), with the
/// intervals shortened.
fn on_interval(
    h: &Harness,
    prober: &Arc<FakeProber>,
    interval: Duration,
) -> Arc<Orchestrator<FakeRuntime>> {
    Orchestrator::with_prober(
        h.store.clone(),
        Arc::clone(&h.runtime),
        Arc::clone(&h.resolver) as Arc<dyn TagResolver>,
        PortAllocator::new(h.store.clone(), PortRange::try_from(20000..=20015).unwrap()),
        h.secrets.clone(),
        Settings {
            monitor: crate::health::MonitorSettings {
                interval,
                min_interval: interval,
                max_interval: interval,
            },
            ..Settings::default()
        },
        Arc::clone(prober) as Arc<dyn Prober>,
    )
}

#[tokio::test]
async fn the_monitor_loop_probes_on_its_interval() {
    let (h, _, prober) = monitored().await;
    let interval = Duration::from_millis(200);
    let orchestrator = on_interval(&h, &prober, interval);
    let mut events = orchestrator.subscribe();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(Arc::clone(&orchestrator).run_health_monitor(cancel.clone()));

    let mut first = Vec::new();
    for _ in 0..3 {
        first.push(next(&mut events).await);
    }
    assert_eq!(
        changes(&first),
        [
            ("api", H::Unknown, H::Healthy),
            ("storage", H::Unknown, H::Healthy),
            ("web", H::Unknown, H::Healthy),
        ]
    );
    let started = Instant::now();
    prober.answer(9000, Err("cannot connect: connection refused"));
    let event = next(&mut events).await;
    assert_eq!(changes(&[event]), [("storage", H::Healthy, H::Unhealthy)]);
    // Picked up by the next probe, on the 200 ms interval (well before the 10 s default).
    assert!(started.elapsed() < interval * 10, "{:?}", started.elapsed());

    cancel.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn a_held_stack_keeps_its_status_and_is_looked_at_again_a_refresh_later() {
    let (_h, orchestrator, prober) = monitored().await;
    let start = Instant::now();
    orchestrator.monitor_at(start).await.unwrap();
    prober.take_probes();

    // A removal holds the stack: no probes, the last status stays, and the next look is a
    // refresh after this pass rather than at a probe time already gone by.
    orchestrator
        .lock()
        .entry("notes".into())
        .or_default()
        .removing = true;
    let later = start + Duration::from_secs(40);
    assert!(orchestrator.monitor_at(later).await.unwrap().is_empty());
    assert!(prober.take_probes().is_empty());
    assert_eq!(health(&orchestrator, "api").await.status, H::Healthy);
    assert_eq!(orchestrator.watchlist().len(), 3);
    assert_eq!(
        orchestrator.next_probe(),
        Some(later + MonitorSettings::default().min_interval)
    );
}

/// A pass that fails (podman unreachable) is retried a refresh later, not at once.
#[tokio::test]
async fn a_failing_pass_does_not_spin_the_loop() {
    let (h, _, prober) = monitored().await;
    let interval = Duration::from_millis(200);
    let orchestrator = on_interval(&h, &prober, interval);
    // Every probe falls due before podman goes away.
    orchestrator.monitor_at(Instant::now()).await.unwrap();
    tokio::time::sleep(interval).await;
    h.runtime.fail_always(
        Operation::ListManaged,
        RuntimeError::Api {
            operation: Operation::ListManaged,
            message: "podman is down".into(),
        },
    );
    let lists = || {
        h.runtime
            .calls()
            .into_iter()
            .filter(|&c| c == Operation::ListManaged)
            .count()
    };
    let before = lists();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(Arc::clone(&orchestrator).run_health_monitor(cancel.clone()));
    tokio::time::sleep(interval * 5).await;
    cancel.cancel();
    task.await.unwrap();
    let passes = lists() - before;
    assert!((1..=8).contains(&passes), "{passes} passes in 1 s");
}
