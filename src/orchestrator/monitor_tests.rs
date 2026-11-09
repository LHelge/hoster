//! Health monitor scenarios (ADR-0038): the orchestrator against [`FakeRuntime`], a real
//! (temporary) store and a fake prober, with passes run at chosen instants.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::error::TryRecvError;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::tests::{FakeProber, Harness, SHOP, harness};
use super::*;
use crate::config::PortRange;
use crate::runtime::{ContainerState, ExecOutcome, FakeRuntime, Operation, RuntimeError};
use crate::store::{EventKind, EventQuery, Severity};

use HealthStatus as H;

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
        Arc::default(),
        Arc::clone(&prober) as Arc<dyn Prober>,
    );
    (h, orchestrator, prober)
}

/// `(service, from, to)` of health events.
fn changes(events: &[UiEvent]) -> Vec<(&str, H, H)> {
    let mut changes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            UiEvent::Health {
                service, from, to, ..
            } => Some((service.as_str(), *from, *to)),
            _ => None,
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
    // The event log (ADR-0055) keeps the change and tells the pages after it.
    assert!(matches!(
        events.try_recv().unwrap(),
        UiEvent::Event(ref event) if event.kind == EventKind::HealthChanged
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
    let UiEvent::Health { detail, .. } = &events[0] else {
        panic!("not a health event: {:?}", events[0]);
    };
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
    // The reconciler's restart and crash-loop events.
    while events.try_recv().is_ok() {}
    let looping = orchestrator
        .monitor_at(start + Duration::from_secs(16))
        .await
        .unwrap();
    // Each restart reset the status to `Unknown` (ADR-0056).
    assert_eq!(changes(&looping), [("api", H::Unknown, H::CrashLooping)]);
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
    // The reconciler wrote the crash loop to the event log; the monitor does not repeat it.
    assert_eq!(events.try_recv().unwrap_err(), TryRecvError::Empty);
    assert!(health_events(&h).await.is_empty());
}

#[tokio::test]
async fn leaving_a_crash_loop_is_one_event_from_the_reconciler() {
    let (h, orchestrator, _prober) = monitored().await;
    let start = Instant::now();
    let at = |secs: f64| start + Duration::from_secs_f64(secs);
    orchestrator.monitor_at(at(0.0)).await.unwrap();

    let api = h.container("hoster-notes-api-g2").id;
    for t in [0.0, 1.0, 3.0, 7.0, 15.0] {
        h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
        orchestrator.reconcile_at(at(t)).await.unwrap();
    }
    let looping = orchestrator.monitor_at(at(16.0)).await.unwrap();
    assert_eq!(changes(&looping), [("api", H::Unknown, H::CrashLooping)]);

    // Up for a whole window: the reconciler resets the history and writes the recovery; the
    // monitor's next passing probe only changes the status.
    orchestrator.reconcile_at(at(15.0 + 600.0)).await.unwrap();
    let recovered = orchestrator.monitor_at(at(16.0 + 600.0)).await.unwrap();
    assert_eq!(changes(&recovered), [("api", H::CrashLooping, H::Healthy)]);
    assert!(health_events(&h).await.is_empty());
    let left = h
        .store
        .event_page(&EventQuery {
            kinds: [EventKind::CrashLoopLeft].into(),
            limit: EventQuery::MAX,
            ..EventQuery::default()
        })
        .await
        .unwrap()
        .events;
    assert_eq!(left.len(), 1);
}

/// The health events of the event log (ADR-0055), oldest first: severity, message, meta.
async fn health_events(h: &Harness) -> Vec<(Severity, String, String)> {
    let mut events: Vec<_> = h
        .store
        .event_page(&EventQuery {
            kinds: [EventKind::HealthChanged].into(),
            limit: EventQuery::MAX,
            ..EventQuery::default()
        })
        .await
        .unwrap()
        .events
        .into_iter()
        .map(|e| {
            assert_eq!(e.stack.as_deref(), Some("notes"));
            (e.severity, e.message, e.meta.unwrap_or_default())
        })
        .collect();
    events.reverse();
    events
}

#[tokio::test]
async fn only_real_health_transitions_reach_the_event_log() {
    let (h, orchestrator, prober) = monitored().await;
    let start = Instant::now();
    let at = |secs: u64| start + Duration::from_secs(secs);

    // The first verdict on a healthy service is no change.
    orchestrator.monitor_at(at(0)).await.unwrap();
    assert!(health_events(&h).await.is_empty());

    // api flaps: down, down, up, up, down. Only the three transitions are written, with the
    // probe detail; a steady state writes nothing however often it is probed.
    for (t, answer) in [
        (15, Err("HTTP 503, expected 2xx-3xx")),
        (30, Err("HTTP 503, expected 2xx-3xx")),
        (45, Ok("HTTP 200")),
        (60, Ok("HTTP 200")),
        (75, Err("cannot connect: connection refused")),
    ] {
        prober.answer(8080, answer);
        orchestrator.monitor_at(at(t)).await.unwrap();
    }
    assert_eq!(
        health_events(&h).await,
        [
            (
                Severity::Warning,
                "notes/api is unhealthy".to_owned(),
                "generation 2 · HTTP 503, expected 2xx-3xx".to_owned()
            ),
            (
                Severity::Success,
                "notes/api is healthy again".to_owned(),
                "generation 2 · HTTP 200".to_owned()
            ),
            (
                Severity::Warning,
                "notes/api is unhealthy".to_owned(),
                "generation 2 · cannot connect: connection refused".to_owned()
            ),
        ]
    );
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
        Arc::default(),
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
        .slots
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

#[tokio::test]
async fn results_of_a_stack_taken_while_its_probes_run_are_dropped() {
    let (_h, orchestrator, prober) = monitored().await;
    let mut ui = orchestrator.subscribe();
    let start = Instant::now();
    // api's probe hangs (as a slow exec would) and would report a failure.
    prober.answer(8080, Err("HTTP 503, expected 2xx-3xx"));
    prober.hold(8080);
    let pass = tokio::spawn({
        let orchestrator = Arc::clone(&orchestrator);
        async move { orchestrator.monitor_at(start).await }
    });
    prober.probed(8080).await;

    // A removal starts while the probes are in flight: it owns the containers now.
    orchestrator
        .lock()
        .slots
        .entry("notes".into())
        .or_default()
        .removing = true;
    prober.release(8080);
    let events = pass.await.unwrap().unwrap();
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(ui.try_recv().unwrap_err(), TryRecvError::Empty);
    for service in ["api", "storage", "web"] {
        let stats = health(&orchestrator, service).await;
        assert_eq!(stats.status, H::Unknown, "{service}");
        assert_eq!((stats.probes, &stats.last), (0, &None), "{service}");
    }
    // The held stack stays watched and is looked at again a refresh later at the earliest.
    assert_eq!(orchestrator.watchlist().len(), 3);
    let next = orchestrator.next_probe().unwrap();
    assert!(next >= start + MonitorSettings::default().min_interval);
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

#[tokio::test]
async fn the_monitor_runs_exec_checks_in_the_live_container() {
    let h = harness().await;
    h.set_secret("shop/db-password", "hunter22").await;
    let applied = h.apply(SHOP).await;
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, DeploymentState::Live, "{deployment:?}");
    let orchestrator = &h.orchestrator;
    let deploy_execs = h.runtime.execs().len();
    h.prober.take_probes();
    let start = Instant::now();
    let at = |secs: u64| start + Duration::from_secs(secs);

    let first = orchestrator.monitor_at(at(0)).await.unwrap();
    assert_eq!(
        changes(&first),
        [
            ("app", H::Unknown, H::Healthy),
            ("db", H::Unknown, H::Healthy)
        ]
    );
    let execs = h.runtime.execs()[deploy_execs..].to_vec();
    assert_eq!(execs.len(), 2, "{execs:?}");
    assert!(execs.contains(&(
        "hoster-shop-db-g1".to_owned(),
        ["pg_isready", "-U", "postgres"].map(str::to_owned).to_vec()
    )));
    let probes = h.prober.take_probes();
    assert_eq!(probes.len(), 1, "app's TCP part only: {probes:?}");
    assert!(probes[0].starts_with("TCP connect to container port 8080 "));

    let stats = health(orchestrator, "db").await;
    assert_eq!(stats.last.as_deref(), Some("exit 0"));
    assert_eq!(
        stats.check.as_deref(),
        Some("exec: pg_isready -U postgres, declared in health.exec")
    );
    assert_eq!(
        stats.detail().as_deref(),
        Some("exit 0 · exec: pg_isready -U postgres, declared in health.exec")
    );
    let app = health(orchestrator, "app").await;
    assert_eq!(app.last.as_deref(), Some("exit 0; TCP: HTTP 200"));

    // A failing exec: unhealthy with its exit code, never its output.
    h.runtime.script_exec(
        "-db-",
        [Ok(ExecOutcome::finished(2, "password hunter22\n".into()))],
    );
    let failing = orchestrator.monitor_at(at(15)).await.unwrap();
    assert_eq!(changes(&failing), [("db", H::Healthy, H::Unhealthy)]);
    assert!(matches!(
        &failing[..],
        [UiEvent::Health { detail, .. }] if detail == "exit 2"
    ));

    // podman cannot run it any more: unhealthy with the reason.
    h.runtime.script_exec(
        "-db-",
        [Ok(ExecOutcome::finished(
            127,
            "Error: runc: exec failed: unable to start container process: exec: \
             \"pg_isready\": executable file not found in $PATH: OCI runtime attempted to \
             invoke a command that was not found\n"
                .into(),
        ))],
    );
    assert!(orchestrator.monitor_at(at(30)).await.unwrap().is_empty());
    assert_eq!(
        health(orchestrator, "db").await.last.as_deref(),
        Some("`health.exec` cannot run: `pg_isready`: executable file not found in $PATH")
    );

    // A timed-out exec fails; at three still running, the monitor starts no fourth.
    let mut guards = Vec::new();
    let mut hung = Vec::new();
    for _ in 0..3 {
        let (exec, guard) = crate::runtime::RunningExec::new();
        guards.push(guard);
        hung.push(Ok(ExecOutcome::TimedOut {
            output: crate::runtime::ExecOutput::default(),
            exec,
        }));
    }
    hung.push(Ok(ExecOutcome::finished(0, "".into())));
    h.runtime.script_exec("-db-", hung);
    let db_execs = || {
        h.runtime
            .execs()
            .iter()
            .filter(|(c, _)| c == "hoster-shop-db-g1")
            .count()
    };
    for n in 0..3 {
        orchestrator.monitor_at(at(45 + 15 * n)).await.unwrap();
        assert_eq!(
            health(orchestrator, "db").await.last.as_deref(),
            Some("no result within 20ms")
        );
    }
    let before = db_execs();
    orchestrator.monitor_at(at(90)).await.unwrap();
    assert_eq!(db_execs(), before, "no exec at the cap");
    assert_eq!(
        health(orchestrator, "db").await.last.as_deref(),
        Some("check stuck: 3 checks still running")
    );
    drop(guards);
    orchestrator.monitor_at(at(105)).await.unwrap();
    assert_eq!(health(orchestrator, "db").await.status, H::Healthy);
}

/// A (re)start resets the status to `Unknown`; failures within min(`start_period`, 5 min)
/// write no event, the first pass wakes the reconciler, and a pass after a restart that
/// interrupted an unhealthy spell is recorded as "healthy again" (ADR-0056 Decision 3).
#[tokio::test]
async fn failures_within_the_grace_after_a_restart_write_no_event() {
    let h = harness().await;
    let live = h
        .finish(
            h.apply(super::tests::DEPS)
                .await
                .deployment
                .as_ref()
                .unwrap(),
        )
        .await;
    assert_eq!(live.state, DeploymentState::Live, "{live:?}");
    let orchestrator = &h.orchestrator;
    h.runtime.script_exec(
        "-db-",
        [
            Ok(ExecOutcome::finished(2, "no\n".into())),
            Ok(ExecOutcome::finished(2, "no\n".into())),
            Ok(ExecOutcome::finished(0, "ok\n".into())),
        ],
    );
    let start = Instant::now();
    let at = |millis: u64| start + Duration::from_millis(millis);

    // First seen: probed at the deploy interval (10 ms), failures keep it `Unknown`.
    let first = orchestrator.monitor_at(at(0)).await.unwrap();
    assert_eq!(changes(&first), [("app", H::Unknown, H::Healthy)]);
    // app's first pass woke the reconciler.
    tokio::time::timeout(
        Duration::from_secs(1),
        orchestrator.reconcile_wake.notified(),
    )
    .await
    .unwrap();
    assert!(orchestrator.monitor_at(at(20)).await.unwrap().is_empty());
    let db = health(orchestrator, "db").await;
    assert_eq!(db.status, H::Unknown);
    assert_eq!((db.probes, db.failures), (2, 2));
    assert!(
        tokio::time::timeout(
            Duration::from_millis(20),
            orchestrator.reconcile_wake.notified()
        )
        .await
        .is_err(),
        "nothing passed"
    );
    // The first pass: healthy, and the reconciler is woken.
    let passed = orchestrator.monitor_at(at(40)).await.unwrap();
    assert_eq!(changes(&passed), [("db", H::Unknown, H::Healthy)]);
    tokio::time::timeout(
        Duration::from_secs(1),
        orchestrator.reconcile_wake.notified(),
    )
    .await
    .unwrap();
    assert!(health_events(&h).await.is_empty());

    // Past the grace a failure is a change, written as before.
    h.runtime
        .script_exec("-db-", [Ok(ExecOutcome::finished(2, "no\n".into()))]);
    let failing = orchestrator.monitor_at(at(60_000)).await.unwrap();
    assert_eq!(changes(&failing), [("db", H::Healthy, H::Unhealthy)]);

    // The reconciler restarts it: `Unknown` again, and its failures are quiet for a while.
    let id = h.container("hoster-notes-db-g1").id;
    h.runtime.set_state(&id, ContainerState::Exited(Some(1)));
    let mut events = orchestrator.subscribe();
    orchestrator.reconcile_at(at(61_000)).await.unwrap();
    assert!(matches!(
        events.try_recv().unwrap(),
        UiEvent::Health { ref service, from: H::Unhealthy, to: H::Unknown, .. } if service == "db"
    ));
    assert_eq!(health(orchestrator, "db").await.status, H::Unknown);
    assert!(
        orchestrator
            .monitor_at(at(61_010))
            .await
            .unwrap()
            .is_empty()
    );
    h.runtime
        .script_exec("-db-", [Ok(ExecOutcome::finished(0, "ok\n".into()))]);
    let recovered = orchestrator.monitor_at(at(61_020)).await.unwrap();
    assert_eq!(changes(&recovered), [("db", H::Unknown, H::Healthy)]);
    let written: Vec<String> = health_events(&h)
        .await
        .into_iter()
        .map(|(_, message, _)| message)
        .collect();
    assert_eq!(
        written,
        ["notes/db is unhealthy", "notes/db is healthy again"]
    );

    // A restart the monitor sees itself (a new `StartedAt`) resets the status too.
    h.runtime
        .set_started_at(&id, std::time::SystemTime::now() + Duration::from_secs(1));
    let reset = orchestrator.monitor_at(at(61_030)).await.unwrap();
    assert!(
        changes(&reset).contains(&("db", H::Healthy, H::Unknown)),
        "{reset:?}"
    );
}

/// The monitor checks with the revision that runs: a later revision whose deployment was
/// refused changes nothing (ADR-0056).
#[tokio::test]
async fn the_monitor_checks_with_the_last_committed_revision() {
    let h = harness().await;
    let live = h
        .finish(
            h.apply(super::tests::DEPS)
                .await
                .deployment
                .as_ref()
                .unwrap(),
        )
        .await;
    assert_eq!(live.state, DeploymentState::Live, "{live:?}");
    // The next revision changes db's check but needs a secret that is not set.
    let refused = super::tests::DEPS
        .replace("exec = [\"pg_isready\"]", "exec = [\"other\"]")
        .replace(
            "update = \"stop-first\"",
            "update = \"stop-first\"\nsecrets = { P = \"notes/password\" }",
        );
    let rejected = h
        .finish(h.apply(&refused).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(rejected.state, DeploymentState::Rejected, "{rejected:?}");
    let before = h.runtime.execs().len();
    h.orchestrator.monitor_at(Instant::now()).await.unwrap();
    assert_eq!(
        h.runtime.execs()[before..],
        [(
            "hoster-notes-db-g1".to_owned(),
            vec!["pg_isready".to_owned()]
        )]
    );
}
