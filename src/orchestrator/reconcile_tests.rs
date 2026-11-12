//! Reconciler scenarios (ADR-0028, architecture §5.4): the orchestrator against [`FakeRuntime`]
//! and a real (temporary) store, with a "restart" being a fresh orchestrator on both.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::tests::{FakeProber, Harness, NOTES, digest, harness, settings};
use super::*;
use crate::config::PortRange;
use crate::runtime::{
    ContainerId, ContainerSpec, ContainerState, ExecOutcome, FakeRuntime, Labels, Operation,
    RuntimeError, ServiceGeneration,
};
use crate::store::{EventKind, EventQuery, NewDeployment, ServicePointer, Severity};

use DeploymentState as S;

fn name(s: &str) -> Name {
    s.parse().unwrap()
}

fn ports(store: &Store) -> PortAllocator {
    PortAllocator::new(store.clone(), PortRange::try_from(20000..=20015).unwrap())
}

/// hoster starting again on the same store and podman.
fn restarted(h: &Harness, restart: RestartPolicy) -> Arc<Orchestrator<FakeRuntime>> {
    Orchestrator::with_prober(
        h.store.clone(),
        Arc::clone(&h.runtime),
        Arc::clone(&h.resolver) as Arc<dyn TagResolver>,
        ports(&h.store),
        h.secrets.clone(),
        Settings {
            restart,
            ..settings(Duration::from_millis(20))
        },
        Arc::default(),
        Arc::new(FakeProber::default()),
    )
}

/// Every service of NOTES live: api g2, web g2, storage g1.
async fn bootstrapped() -> Harness {
    let h = harness().await;
    h.bootstrap().await;
    h
}

/// Creates and starts `hoster-<stack>-<service>-g<generation>` as deployment `deployment`
/// would have, network and image included.
async fn container(
    h: &Harness,
    stack: &str,
    service: &str,
    generation: u32,
    deployment: DeploymentId,
) -> ContainerId {
    let generation = ServiceGeneration {
        stack: name(stack),
        service: name(service),
        generation,
    };
    let image: PinnedImage = format!("ghcr.io/acme/{stack}-{service}@{}", digest(3))
        .parse()
        .unwrap();
    h.runtime.pull_image(&image).await.unwrap();
    h.runtime
        .ensure_network(&NetworkSpec::for_stack(&generation.stack))
        .await
        .unwrap();
    let spec = ContainerSpec::new(&generation, deployment, image);
    let id = h.runtime.create_container(&spec).await.unwrap();
    h.runtime.start(&id).await.unwrap();
    id
}

/// What a crash left behind: a deployment of api at digest 3 that reached `state`, with the
/// work of every state before it done. Returns it with its reserved port.
async fn interrupted(h: &Harness, state: DeploymentState) -> (Deployment, u16) {
    let revision = h.store.latest_revision("notes").await.unwrap().unwrap();
    let record = h
        .store
        .create_deployment(&NewDeployment {
            stack: "notes".into(),
            revision: Some(revision.id),
            source: DeploymentSource::Webhook,
            target: DigestSet::from([("api".into(), digest(3))]),
            provenance: None,
        })
        .await
        .unwrap();
    let path = [
        S::Validating,
        S::Pulling,
        S::Starting,
        S::HealthChecking,
        S::Replacing,
        S::Switching,
        S::Draining,
        S::CleaningUp,
    ];
    let reached = path.iter().position(|&s| s == state).unwrap();
    let generation = h.store.assign_generation(record.id).await.unwrap();
    let port = ports(&h.store)
        .allocate("notes", "api", 8080, generation)
        .await
        .unwrap();
    for &step in &path[..=reached] {
        h.store.transition(record.id, step, None).await.unwrap();
        match step {
            S::Starting => {
                container(h, "notes", "api", generation, record.id).await;
            }
            S::Replacing => {
                // `Replacing` stopped the old stop-first container.
                let storage = h.container("hoster-notes-storage-g1");
                h.runtime
                    .stop(&storage.id, Duration::from_secs(1))
                    .await
                    .unwrap();
            }
            S::Draining => {
                let api = h
                    .store
                    .services("notes")
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|p| p.service == "api")
                    .unwrap();
                let pointer = ServicePointer {
                    generation,
                    digest: digest(3),
                    ports: [(8080, port)].into(),
                    ..api
                };
                let succeeded = event_log::DeployEvents::from(&record)
                    .succeeded(crate::store::Timestamp::now());
                h.store
                    .commit_recorded(record.id, &[pointer], &[], &succeeded)
                    .await
                    .unwrap();
            }
            _ => {}
        }
    }
    let deployment = h.store.deployment(record.id).await.unwrap().unwrap();
    (deployment, port)
}

async fn allocated(h: &Harness) -> Vec<(String, u32)> {
    ports(&h.store)
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|a| (a.service, a.generation))
        .collect()
}

#[tokio::test]
async fn interrupted_before_starting_fails_and_releases_its_reservations() {
    for state in [S::Validating, S::Pulling] {
        let h = bootstrapped().await;
        let before = h.container_names();
        let (deployment, _) = interrupted(&h, state).await;
        assert!(allocated(&h).await.contains(&("api".into(), 3)));

        let report = restarted(&h, RestartPolicy::default())
            .recover()
            .await
            .unwrap();
        assert_eq!(report.recovered, [(deployment.id, S::Failed)], "{state}");
        let after = h.store.deployment(deployment.id).await.unwrap().unwrap();
        assert_eq!(after.state, S::Failed);
        assert_eq!(
            after.error.as_deref(),
            Some("interrupted by a hoster restart")
        );
        assert!(!allocated(&h).await.contains(&("api".into(), 3)), "{state}");
        assert_eq!(report.ports_released.len(), 1, "{state}");
        assert_eq!(h.container_names(), before, "{state}");
        assert_eq!(report.adopted, before, "{state}");
        assert_eq!(
            h.generations().await,
            [("api".into(), 2), ("storage".into(), 1), ("web".into(), 2)]
        );
    }
}

/// The deploy events (ADR-0055) of deployment `id`, oldest first.
async fn deploy_events(h: &Harness, id: DeploymentId) -> Vec<(EventKind, Severity)> {
    let subject = format!("/stacks/notes/deployments/{id}");
    let mut events: Vec<_> = h
        .store
        .event_page(&EventQuery {
            stack: Some("notes".into()),
            limit: EventQuery::MAX,
            ..EventQuery::default()
        })
        .await
        .unwrap()
        .events
        .into_iter()
        .filter(|e| e.subject.as_deref() == Some(subject.as_str()))
        .map(|e| (e.kind, e.severity))
        .collect();
    events.reverse();
    events
}

#[tokio::test]
async fn recovery_writes_an_interrupted_deployments_outcome_exactly_once() {
    // Before the commit the recovery fails the deployment and writes that; after it, the
    // success the commit wrote stands and recovery writes nothing.
    for (state, outcome) in [
        (S::Validating, (EventKind::DeployFailed, Severity::Error)),
        (S::Starting, (EventKind::DeployFailed, Severity::Error)),
        (S::Draining, (EventKind::DeploySucceeded, Severity::Success)),
    ] {
        let h = bootstrapped().await;
        let (deployment, _) = interrupted(&h, state).await;
        restarted(&h, RestartPolicy::default())
            .recover()
            .await
            .unwrap();
        // A second restart finds nothing unfinished and writes nothing more.
        restarted(&h, RestartPolicy::default())
            .recover()
            .await
            .unwrap();
        assert_eq!(deploy_events(&h, deployment.id).await, [outcome], "{state}");
    }
}

#[tokio::test]
async fn interrupted_before_the_commit_removes_the_new_generation() {
    // `Switching` with the pointers not yet written is treated like `HealthChecking` (§5.4).
    for state in [S::Starting, S::HealthChecking, S::Replacing, S::Switching] {
        let h = bootstrapped().await;
        let (deployment, _) = interrupted(&h, state).await;
        assert!(
            h.container_names()
                .contains(&"hoster-notes-api-g3".to_owned())
        );

        let report = restarted(&h, RestartPolicy::default())
            .recover()
            .await
            .unwrap();
        assert_eq!(report.recovered, [(deployment.id, S::Failed)], "{state}");
        assert_eq!(report.removed, ["hoster-notes-api-g3"], "{state}");
        assert_eq!(
            h.container_names(),
            [
                "hoster-notes-api-g2",
                "hoster-notes-storage-g1",
                "hoster-notes-web-g2"
            ],
            "{state}"
        );
        // The stop-first container `Replacing` stopped serves again.
        assert_eq!(
            h.container("hoster-notes-storage-g1").state,
            ContainerState::Running,
            "{state}"
        );
        if state == S::Replacing || state == S::Switching {
            assert_eq!(report.started, ["hoster-notes-storage-g1"], "{state}");
        }
        assert!(!allocated(&h).await.contains(&("api".into(), 3)), "{state}");
        assert_eq!(
            h.generations().await,
            [("api".into(), 2), ("storage".into(), 1), ("web".into(), 2)],
            "{state}"
        );
    }
}

#[tokio::test]
async fn interrupted_after_the_commit_keeps_the_new_generation() {
    for state in [S::Draining, S::CleaningUp] {
        let h = bootstrapped().await;
        let (deployment, port) = interrupted(&h, state).await;
        assert!(deployment.committed_at.is_some());

        let report = restarted(&h, RestartPolicy::default())
            .recover()
            .await
            .unwrap();
        assert_eq!(report.recovered, [(deployment.id, S::Live)], "{state}");
        assert_eq!(report.removed, ["hoster-notes-api-g2"], "{state}");
        assert_eq!(
            h.container_names(),
            [
                "hoster-notes-api-g3",
                "hoster-notes-storage-g1",
                "hoster-notes-web-g2"
            ],
            "{state}"
        );
        assert_eq!(
            h.generations().await,
            [("api".into(), 3), ("storage".into(), 1), ("web".into(), 2)]
        );
        // The previous generation's port goes; the committed one stays.
        let api_ports: Vec<_> = ports(&h.store)
            .list()
            .await
            .unwrap()
            .into_iter()
            .filter(|a| a.service == "api")
            .map(|a| (a.generation, a.port))
            .collect();
        assert_eq!(api_ports, [(3, port)], "{state}");
        let after = h.store.deployment(deployment.id).await.unwrap().unwrap();
        assert_eq!(after.state, S::Live);
    }
}

#[tokio::test]
async fn startup_adopts_running_starts_stopped_and_removes_old_generations() {
    let h = bootstrapped().await;
    // A reboot: nothing runs. A leftover of an old generation survived a failed clean-up.
    for c in h.runtime.containers() {
        h.runtime.set_state(&c.id, ContainerState::Exited(Some(0)));
    }
    let web = h.container("hoster-notes-web-g2");
    h.runtime.start(&web.id).await.unwrap();
    let previous = h.store.deployments("notes", 1).await.unwrap()[0].id;
    container(&h, "notes", "api", 1, previous).await;

    let report = restarted(&h, RestartPolicy::default())
        .recover()
        .await
        .unwrap();
    assert!(report.recovered.is_empty());
    assert_eq!(report.adopted, ["hoster-notes-web-g2"]);
    assert_eq!(
        report.started,
        ["hoster-notes-api-g2", "hoster-notes-storage-g1"]
    );
    assert_eq!(report.removed, ["hoster-notes-api-g1"]);
    assert!(
        h.runtime
            .containers()
            .iter()
            .all(|c| c.state == ContainerState::Running)
    );
}

#[tokio::test]
async fn orphans_of_removed_stacks_go_and_foreign_objects_stay() {
    let h = bootstrapped().await;
    // A stack removed while hoster was down: its container and network are orphans.
    container(&h, "gone", "web", 4, DeploymentId::from(999)).await;
    // `hoster.managed=true`, but not hoster's: no stack label, or a name that does not match.
    let image: PinnedImage = format!("ghcr.io/acme/x@{}", digest(3)).parse().unwrap();
    let generation = ServiceGeneration {
        stack: name("notes"),
        service: name("web"),
        generation: 2,
    };
    let mut unlabelled = ContainerSpec::new(&generation, DeploymentId::from(1), image.clone());
    unlabelled.name = "someone-elses".into();
    unlabelled.labels.remove(label::STACK);
    h.runtime.pull_image(&image).await.unwrap();
    let mut renamed = ContainerSpec::new(&generation, DeploymentId::from(1), image);
    renamed.name = "hoster-notes-web-copy".into();
    for spec in [&unlabelled, &renamed] {
        let id = h.runtime.create_container(spec).await.unwrap();
        h.runtime.set_state(&id, ContainerState::Exited(Some(1)));
    }
    h.runtime
        .ensure_network(&NetworkSpec {
            name: "hoster-borrowed".into(),
            labels: Labels::from([(label::MANAGED.into(), "true".into())]),
        })
        .await
        .unwrap();

    let orchestrator = restarted(&h, RestartPolicy::default());
    let report = orchestrator.recover().await.unwrap();
    assert_eq!(report.removed, ["hoster-gone-web-g4"]);
    assert_eq!(report.networks_removed, ["hoster-gone"]);
    assert_eq!(
        report.foreign,
        ["hoster-notes-web-copy", "someone-elses", "hoster-borrowed"]
    );
    let names = h.container_names();
    assert!(names.contains(&"someone-elses".to_owned()));
    assert!(names.contains(&"hoster-notes-web-copy".to_owned()));
    assert!(!names.contains(&"hoster-gone-web-g4".to_owned()));
    // Left alone: neither started nor removed.
    assert_eq!(
        h.container("someone-elses").state,
        ContainerState::Exited(Some(1))
    );
    let networks: Vec<_> = h.runtime.networks().into_iter().map(|n| n.name).collect();
    assert_eq!(networks, ["hoster-borrowed", "hoster-notes"]);

    // The periodic pass leaves them alone too.
    let report = orchestrator.reconcile().await.unwrap();
    assert_eq!(report.foreign.len(), 3);
    assert!(report.removed.is_empty() && report.started.is_empty());
}

#[tokio::test]
async fn exited_containers_restart_with_backoff_until_crash_looping() {
    let h = bootstrapped().await;
    let orchestrator = restarted(&h, RestartPolicy::default());
    orchestrator.recover().await.unwrap();
    let api = h.container("hoster-notes-api-g2").id;
    let start = Instant::now();
    let at = |secs: f64| start + Duration::from_secs_f64(secs);
    let crash = || h.runtime.set_state(&api, ContainerState::Exited(Some(1)));

    // Restarts at 0 s, then after 1, 2, 4 and 8 s: a pass before the wait is over defers it.
    let mut restarted_at = Vec::new();
    for t in [0.0, 0.5, 1.0, 1.5, 3.0, 6.0, 7.0, 14.5, 15.0] {
        crash();
        let report = orchestrator.reconcile_at(at(t)).await.unwrap();
        if report.started == ["hoster-notes-api-g2"] {
            restarted_at.push(t);
        } else {
            assert_eq!(report.backing_off, ["hoster-notes-api-g2"], "at {t}");
            h.runtime.start(&api).await.unwrap();
        }
        assert_eq!(
            h.container("hoster-notes-api-g2").state,
            ContainerState::Running,
            "at {t}"
        );
    }
    assert_eq!(restarted_at, [0.0, 1.0, 3.0, 7.0, 15.0]);

    // Five restarts within ten minutes: crash looping, retried every five minutes.
    let status = orchestrator.status().await.unwrap();
    let api_status = status[0].services.iter().find(|s| s.name == "api").unwrap();
    assert_eq!(
        api_status.restarts,
        RestartStats {
            restarts: 5,
            crash_looping: true,
            last_exit: Some(ExitReason::Exit(ContainerState::Exited(Some(1)))),
        }
    );
    crash();
    let report = orchestrator.reconcile_at(at(15.0 + 299.0)).await.unwrap();
    assert_eq!(report.backing_off, ["hoster-notes-api-g2"]);
    let report = orchestrator.reconcile_at(at(15.0 + 300.0)).await.unwrap();
    assert_eq!(report.started, ["hoster-notes-api-g2"]);
    // The other services were adopted at every pass, never restarted.
    assert_eq!(
        report.adopted,
        ["hoster-notes-storage-g1", "hoster-notes-web-g2"]
    );
}

#[tokio::test]
async fn stacks_with_an_active_deployment_are_skipped() {
    let h = bootstrapped().await;
    let orchestrator = restarted(&h, RestartPolicy::default());
    let api = h.container("hoster-notes-api-g2").id;
    h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
    let previous = h.store.deployments("notes", 1).await.unwrap()[0].id;
    container(&h, "notes", "api", 7, previous).await;

    let busy = DeploymentId::from(12345);
    assert!(matches!(
        orchestrator.claim("notes", busy, S::Starting),
        Claim::Free
    ));
    let report = orchestrator.reconcile().await.unwrap();
    assert_eq!(report.skipped_stacks, ["notes"]);
    assert!(report.started.is_empty() && report.removed.is_empty());
    assert_eq!(
        h.container("hoster-notes-api-g2").state,
        ContainerState::Exited(Some(1))
    );

    orchestrator.lock().slots.remove("notes");
    let report = orchestrator.reconcile().await.unwrap();
    assert_eq!(report.started, ["hoster-notes-api-g2"]);
    assert_eq!(report.removed, ["hoster-notes-api-g7"]);
}

#[tokio::test]
async fn the_reconcile_loop_restarts_crashed_containers_until_cancelled() {
    let h = harness().await;
    let applied = h.apply(NOTES).await;
    h.finish(applied.deployment.as_ref().unwrap()).await;
    let orchestrator = restarted(
        &h,
        RestartPolicy {
            interval: Duration::from_millis(20),
            initial_backoff: Duration::from_millis(5),
            ..RestartPolicy::default()
        },
    );
    let cancel = CancellationToken::new();
    let task = tokio::spawn(Arc::clone(&orchestrator).run_reconciler(cancel.clone()));
    let storage = h.container("hoster-notes-storage-g1").id;
    for _ in 0..3 {
        h.runtime
            .set_state(&storage, ContainerState::Exited(Some(137)));
        tokio::time::timeout(Duration::from_secs(5), async {
            while h.container("hoster-notes-storage-g1").state != ContainerState::Running {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the loop restarts the container");
    }
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the loop stops when cancelled")
        .unwrap();
    assert_eq!(
        orchestrator.restart_stats("notes", "storage", 1).restarts,
        3
    );
}

#[tokio::test]
async fn reconcile_actions_log_stack_service_and_generation() {
    use crate::logging::LogFormat;
    use crate::logging::capture::subscriber;

    let h = bootstrapped().await;
    let orchestrator = restarted(&h, RestartPolicy::default());
    let api = h.container("hoster-notes-api-g2").id;
    h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
    let previous = h.store.deployments("notes", 1).await.unwrap()[0].id;
    container(&h, "notes", "web", 1, previous).await;

    let (buffer, subscriber) = subscriber(LogFormat::Json, "hoster=info");
    let guard = tracing::subscriber::set_default(subscriber);
    orchestrator.reconcile().await.unwrap();
    drop(guard);

    let lines = buffer.json();
    let find = |message: &str| {
        lines
            .iter()
            .find(|l| l["message"] == message)
            .unwrap_or_else(|| panic!("no {message:?} in {lines:?}"))
            .clone()
    };
    let restarted = find("restarted exited container");
    assert_eq!(
        (
            &restarted["stack"],
            &restarted["service"],
            &restarted["generation"]
        ),
        (
            &serde_json::json!("notes"),
            &serde_json::json!("api"),
            &serde_json::json!(2)
        ),
        "{restarted}"
    );
    let removed = find("removing container");
    assert_eq!(removed["service"], "web", "{removed}");
    assert_eq!(removed["generation"], 1, "{removed}");
}

#[tokio::test]
async fn startup_removes_the_aborted_generation_before_starting_the_stopped_one() {
    // A stop-first service mid-`Replacing`: the old generation stopped, the new one running.
    // Both must never run at once, so the new one goes before the old one starts.
    let h = bootstrapped().await;
    let storage = h.container("hoster-notes-storage-g1");
    h.runtime
        .stop(&storage.id, Duration::from_secs(1))
        .await
        .unwrap();
    container(&h, "notes", "storage", 3, DeploymentId::from(999)).await;
    let before = h.runtime.calls().len();

    let report = restarted(&h, RestartPolicy::default())
        .recover()
        .await
        .unwrap();
    assert_eq!(report.removed, ["hoster-notes-storage-g3"]);
    assert_eq!(report.started, ["hoster-notes-storage-g1"]);
    let acted: Vec<_> = h.runtime.calls()[before..]
        .iter()
        .copied()
        .filter(|op| matches!(op, Operation::Stop | Operation::Remove | Operation::Start))
        .collect();
    assert_eq!(
        acted,
        [Operation::Stop, Operation::Remove, Operation::Start]
    );
}

#[tokio::test]
async fn a_deployment_submitted_during_a_pass_waits_for_it() {
    let h = bootstrapped().await;
    let claim = h
        .orchestrator
        .reconcile_claim("notes")
        .expect("the slot is free");
    assert!(
        h.orchestrator.reconcile_claim("notes").is_none(),
        "one pass at a time"
    );

    let outcome = h.deploy(&[("api", 4)]).await;
    assert_eq!(outcome.state, S::Queued);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let queued = h
        .store
        .deployment(outcome.deployment)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        queued.state,
        S::Queued,
        "nothing runs while the pass holds the stack"
    );
    assert!(
        !h.container_names()
            .contains(&"hoster-notes-api-g3".to_owned())
    );

    drop(claim);
    let deployment = h.finish(&outcome).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    assert!(
        h.container_names()
            .contains(&"hoster-notes-api-g3".to_owned())
    );
    // The finished deployment freed the slot again.
    assert!(h.orchestrator.reconcile_claim("notes").is_some());
}

#[tokio::test]
async fn a_removal_waits_for_the_pass_and_then_blocks_passes() {
    let h = bootstrapped().await;
    let claim = h
        .orchestrator
        .reconcile_claim("notes")
        .expect("the slot is free");
    let orchestrator = Arc::clone(&h.orchestrator);
    let removal = tokio::spawn(async move {
        orchestrator
            .remove(RemoveStack::keeping_volumes("notes"))
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!removal.is_finished(), "the removal waits for the pass");
    assert_eq!(h.container_names().len(), 3);
    assert!(
        h.orchestrator.is_removing("notes"),
        "marked before it waits, so no new pass starts"
    );

    drop(claim);
    let outcome = tokio::time::timeout(Duration::from_secs(5), removal)
        .await
        .expect("the removal goes on once the pass is done")
        .unwrap()
        .unwrap();
    assert_eq!(outcome.containers, 3);
    assert!(h.container_names().is_empty());
}

#[tokio::test]
async fn stacks_being_removed_are_skipped() {
    let h = bootstrapped().await;
    h.orchestrator
        .lock()
        .slots
        .entry("notes".into())
        .or_default()
        .removing = true;
    assert!(h.orchestrator.reconcile_claim("notes").is_none());
    let report = h.orchestrator.reconcile().await.unwrap();
    assert_eq!(report.skipped_stacks, ["notes"]);
    assert!(report.adopted.is_empty());
}

/// The container lifecycle events (ADR-0055) of the event log, oldest first: kind, severity,
/// message and meta.
async fn lifecycle_events(h: &Harness) -> Vec<(EventKind, Severity, String, String)> {
    let mut events: Vec<_> = h
        .store
        .event_page(&EventQuery {
            kinds: [
                EventKind::ContainerRestarted,
                EventKind::CrashLoopEntered,
                EventKind::CrashLoopLeft,
            ]
            .into(),
            limit: EventQuery::MAX,
            ..EventQuery::default()
        })
        .await
        .unwrap()
        .events
        .into_iter()
        .map(|e| {
            assert_eq!(e.stack.as_deref(), Some("notes"));
            assert_eq!(e.subject.as_deref(), Some("/stacks/notes"));
            (e.kind, e.severity, e.message, e.meta.unwrap_or_default())
        })
        .collect();
    events.reverse();
    events
}

#[tokio::test]
async fn startup_starts_the_current_generation_without_restart_events() {
    let h = bootstrapped().await;
    // A reboot: nothing runs.
    for c in h.runtime.containers() {
        h.runtime.set_state(&c.id, ContainerState::Exited(Some(0)));
    }
    let report = restarted(&h, RestartPolicy::default())
        .recover()
        .await
        .unwrap();
    assert_eq!(report.started.len(), 3);
    assert!(lifecycle_events(&h).await.is_empty());
}

#[tokio::test]
async fn each_restart_writes_one_event_with_the_exit_code_and_oom() {
    let h = bootstrapped().await;
    let orchestrator = restarted(&h, RestartPolicy::default());
    orchestrator.recover().await.unwrap();
    let api = h.container("hoster-notes-api-g2").id;
    let start = Instant::now();

    h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
    orchestrator.reconcile_at(start).await.unwrap();
    // Still backing off: no restart, no event; and a pass that only adopts writes none.
    h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
    let report = orchestrator
        .reconcile_at(start + Duration::from_millis(500))
        .await
        .unwrap();
    assert_eq!(report.backing_off, ["hoster-notes-api-g2"]);
    h.runtime.oom_kill(&api);
    orchestrator
        .reconcile_at(start + Duration::from_secs(1))
        .await
        .unwrap();
    orchestrator
        .reconcile_at(start + Duration::from_secs(2))
        .await
        .unwrap();

    assert_eq!(
        lifecycle_events(&h).await,
        [
            (
                EventKind::ContainerRestarted,
                Severity::Warning,
                "Restarted notes/api: exit 1".to_owned(),
                "generation 2 · attempt 1".to_owned()
            ),
            (
                EventKind::ContainerRestarted,
                Severity::Warning,
                "Restarted notes/api: OOM-killed (no memory limit)".to_owned(),
                "generation 2 · attempt 2".to_owned()
            ),
        ]
    );
    // The OOM verdict comes from inspect: the listing, as podman's, never says so.
    assert!(h.runtime.calls().contains(&Operation::Inspect));
}

#[tokio::test]
async fn entering_and_leaving_crash_loop_are_events() {
    let h = bootstrapped().await;
    let orchestrator = restarted(&h, RestartPolicy::default());
    orchestrator.recover().await.unwrap();
    let api = h.container("hoster-notes-api-g2").id;
    let start = Instant::now();
    let at = |secs: f64| start + Duration::from_secs_f64(secs);

    // Five restarts on the backoff (0, 1, 3, 7, 15 s): crash looping at the fifth.
    for t in [0.0, 1.0, 3.0, 7.0, 15.0] {
        h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
        let report = orchestrator.reconcile_at(at(t)).await.unwrap();
        assert_eq!(report.started, ["hoster-notes-api-g2"], "at {t}");
    }
    // Up, but the last restart is still in the window: still crash looping.
    orchestrator.reconcile_at(at(15.0 + 300.0)).await.unwrap();
    // Up for a whole window since: the history resets and the crash loop is over, once.
    orchestrator.reconcile_at(at(15.0 + 600.0)).await.unwrap();
    orchestrator.reconcile_at(at(15.0 + 615.0)).await.unwrap();

    let events = lifecycle_events(&h).await;
    let kinds: Vec<_> = events.iter().map(|e| e.0).collect();
    assert_eq!(
        kinds,
        [
            EventKind::ContainerRestarted,
            EventKind::ContainerRestarted,
            EventKind::ContainerRestarted,
            EventKind::ContainerRestarted,
            EventKind::ContainerRestarted,
            EventKind::CrashLoopEntered,
            EventKind::CrashLoopLeft,
        ]
    );
    assert_eq!(
        events[5..],
        [
            (
                EventKind::CrashLoopEntered,
                Severity::Error,
                "notes/api is crash looping".to_owned(),
                "generation 2 · 5 restarts in 10m".to_owned()
            ),
            (
                EventKind::CrashLoopLeft,
                Severity::Success,
                "notes/api stopped crash looping".to_owned(),
                "generation 2".to_owned()
            ),
        ]
    );
}

// Service dependencies (ADR-0056 Decision 3).

/// DEPS (app → db) live, with `db`'s check given `start_period`.
async fn dependent(start_period: &str) -> Harness {
    let h = harness().await;
    let stack = super::tests::DEPS.replace(
        "interval = \"10ms\"",
        &format!("interval = \"10ms\"\nstart_period = \"{start_period}\""),
    );
    let live = h
        .finish(h.apply(&stack).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(live.state, S::Live, "{live:?}");
    h
}

fn stopped(h: &Harness, name: &str) {
    let id = h.container(name).id;
    h.runtime.set_state(&id, ContainerState::Exited(Some(1)));
}

async fn service_status(
    orchestrator: &Orchestrator<FakeRuntime>,
    service: &str,
) -> super::ServiceStatus {
    orchestrator.status().await.unwrap()[0]
        .services
        .iter()
        .find(|s| s.name == service)
        .unwrap()
        .clone()
}

/// A dependency that is not running holds its dependents without a bound and at no
/// crash-loop cost; once it runs and passes, the dependent starts.
#[tokio::test]
async fn a_stopped_dependency_holds_its_dependents_at_no_crash_loop_cost() {
    let h = dependent("30s").await;
    let orchestrator = &h.orchestrator;
    stopped(&h, "hoster-notes-db-g1");
    stopped(&h, "hoster-notes-app-g1");
    // db cannot start, again and again.
    h.runtime.fail_always(
        Operation::Start,
        RuntimeError::Api {
            operation: Operation::Start,
            message: "no".into(),
        },
    );
    let start = Instant::now();
    for i in 0..8 {
        let report = orchestrator
            .reconcile_at(start + Duration::from_secs((1 << i) - 1))
            .await
            .unwrap();
        assert_eq!(report.waiting, ["hoster-notes-app-g1"], "pass {i}");
    }
    let app = service_status(orchestrator, "app").await;
    assert_eq!(app.waiting_for, ["db"]);
    assert_eq!(app.restarts, RestartStats::default());
    assert!(
        service_status(orchestrator, "db")
            .await
            .restarts
            .crash_looping
    );

    // db starts, but has not passed yet: app still waits.
    h.runtime.clear_failures();
    let at = start + Duration::from_secs(4000);
    let report = orchestrator.reconcile_at(at).await.unwrap();
    assert_eq!(report.started, ["hoster-notes-db-g1"]);
    assert_eq!(report.waiting, ["hoster-notes-app-g1"]);
    // Once it passes, app starts with its usual restart record.
    orchestrator
        .monitor_at(at + Duration::from_millis(1))
        .await
        .unwrap();
    let report = orchestrator
        .reconcile_at(at + Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(report.started, ["hoster-notes-app-g1"]);
    assert!(report.waiting.is_empty());
    let app = service_status(orchestrator, "app").await;
    assert!(app.waiting_for.is_empty());
    assert_eq!(app.restarts.restarts, 1);
    assert!(h.container("hoster-notes-app-g1").state.is_running());
}

/// A dependency that runs but has not passed within min(`start_period`, 5 min) stops holding:
/// the dependent starts, and one event per hold says so.
#[tokio::test]
async fn a_hold_on_a_running_dependency_ends_after_its_start_period_with_one_event() {
    let h = dependent("30s").await;
    let orchestrator = &h.orchestrator;
    h.runtime
        .script_exec("-db-", [Ok(ExecOutcome::finished(2, "no\n".into()))]);
    stopped(&h, "hoster-notes-app-g1");
    let start = Instant::now();
    orchestrator.monitor_at(start).await.unwrap();
    let report = orchestrator
        .reconcile_at(start + Duration::from_secs(29))
        .await
        .unwrap();
    assert_eq!(report.waiting, ["hoster-notes-app-g1"]);
    // The reconcile loop wakes when the hold ends, not at its next tick.
    assert_eq!(
        orchestrator.next_hold_end(start + Duration::from_secs(29)),
        Some(start + Duration::from_secs(30))
    );

    let report = orchestrator
        .reconcile_at(start + Duration::from_secs(31))
        .await
        .unwrap();
    assert_eq!(report.started, ["hoster-notes-app-g1"]);
    // It keeps crashing during the same hold: its restarts write only their usual events.
    stopped(&h, "hoster-notes-app-g1");
    let report = orchestrator
        .reconcile_at(start + Duration::from_secs(40))
        .await
        .unwrap();
    assert_eq!(report.started, ["hoster-notes-app-g1"]);
    let events = lifecycle_events(&h).await;
    let past: Vec<_> = events.iter().filter(|e| e.2.contains("although")).collect();
    assert_eq!(
        past,
        [&(
            EventKind::ContainerRestarted,
            Severity::Warning,
            "Started notes/app although db has not passed its check".to_owned(),
            "generation 1".to_owned()
        )]
    );
    assert_eq!(events.len(), 3, "two restarts and the hold: {events:#?}");
    assert_eq!(
        service_status(orchestrator, "app").await.restarts.restarts,
        2
    );
}

/// At startup, a container whose dependencies are not ready is left to the periodic pass,
/// which starts it as a startup start once they are: no restart record, no event.
#[tokio::test]
async fn startup_defers_containers_whose_dependencies_are_not_ready() {
    let h = dependent("30s").await;
    // A reboot: nothing runs.
    stopped(&h, "hoster-notes-db-g1");
    stopped(&h, "hoster-notes-app-g1");
    let orchestrator = restarted(&h, RestartPolicy::default());
    let report = orchestrator.recover().await.unwrap();
    assert_eq!(report.started, ["hoster-notes-db-g1"]);
    assert_eq!(report.waiting, ["hoster-notes-app-g1"]);
    assert!(!h.container("hoster-notes-app-g1").state.is_running());

    let start = Instant::now();
    let report = orchestrator.reconcile_at(start).await.unwrap();
    assert_eq!(report.waiting, ["hoster-notes-app-g1"]);
    // db passes; the monitor wakes the reconcile loop.
    orchestrator
        .monitor_at(start + Duration::from_millis(1))
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        orchestrator.reconcile_wake.notified(),
    )
    .await
    .expect("the first pass wakes the reconciler");
    let report = orchestrator
        .reconcile_at(start + Duration::from_millis(2))
        .await
        .unwrap();
    assert_eq!(report.started, ["hoster-notes-app-g1"]);
    assert!(h.container("hoster-notes-app-g1").state.is_running());
    assert_eq!(
        service_status(&orchestrator, "app").await.restarts,
        RestartStats::default()
    );
    assert!(lifecycle_events(&h).await.is_empty());
}

/// A deferred startup start past its hold is still a startup start, with the hold's event.
#[tokio::test]
async fn a_deferred_startup_start_past_its_hold_writes_the_hold_event() {
    let h = dependent("1s").await;
    stopped(&h, "hoster-notes-app-g1");
    h.runtime
        .script_exec("-db-", [Ok(ExecOutcome::finished(2, "no\n".into()))]);
    let orchestrator = restarted(&h, RestartPolicy::default());
    let start = Instant::now();
    let report = orchestrator.recover().await.unwrap();
    assert_eq!(report.waiting, ["hoster-notes-app-g1"]);
    let report = orchestrator
        .reconcile_at(start + Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(report.started, ["hoster-notes-app-g1"]);
    assert_eq!(
        service_status(&orchestrator, "app").await.restarts,
        RestartStats::default()
    );
    let events = lifecycle_events(&h).await;
    assert_eq!(events.len(), 1, "{events:#?}");
    assert!(events[0].2.contains("although db"), "{events:#?}");
}

/// A `depends_on` service without a pointer (a CI-built one that never got a digest) is
/// ignored: the dependent starts at once.
#[tokio::test]
async fn a_dependency_without_a_pointer_holds_nothing() {
    let h = harness().await;
    let stack = super::tests::DEPS.replace("depends_on = [\"db\"]", "depends_on = [\"ci\"]")
        + "\n[services.ci]\nimage = \"ghcr.io/acme/ci\"\n";
    let applied = h.apply(&stack).await;
    assert_eq!(applied.changed, ["app", "db"]);
    let live = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");
    stopped(&h, "hoster-notes-app-g1");
    let report = h.orchestrator.reconcile_at(Instant::now()).await.unwrap();
    assert_eq!(report.started, ["hoster-notes-app-g1"]);
    assert!(report.waiting.is_empty(), "{report:?}");
    assert!(
        service_status(&h.orchestrator, "app")
            .await
            .waiting_for
            .is_empty()
    );
}

/// An unrouted dependency without a health table is checked by running for the settle period:
/// it holds its dependents until it has run that long since it was first seen running, and the
/// first pass after that starts them as ready, with no monitor probe in between and no
/// past-hold event (ADR-0056 Decision 3).
#[tokio::test]
async fn a_settle_checked_dependency_passes_by_running_for_its_period() {
    let h = harness().await;
    let stack = super::tests::DEPS.replace(
        "\n[services.db.health]\nexec = [\"pg_isready\"]\ninterval = \"10ms\"\n",
        "",
    );
    assert!(!stack.contains("[services.db.health]"));
    let live = h
        .finish(h.apply(&stack).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(live.state, S::Live, "{live:?}");
    let orchestrator = &h.orchestrator;
    stopped(&h, "hoster-notes-app-g1");
    // The harness settles for 20 ms; db is first seen running now.
    let start = Instant::now();
    let report = orchestrator.reconcile_at(start).await.unwrap();
    assert_eq!(report.waiting, ["hoster-notes-app-g1"]);
    assert_eq!(
        service_status(orchestrator, "app").await.waiting_for,
        ["db"]
    );
    let report = orchestrator
        .reconcile_at(start + Duration::from_millis(10))
        .await
        .unwrap();
    assert_eq!(report.waiting, ["hoster-notes-app-g1"]);

    let report = orchestrator
        .reconcile_at(start + Duration::from_millis(21))
        .await
        .unwrap();
    assert_eq!(report.started, ["hoster-notes-app-g1"]);
    assert!(report.waiting.is_empty());
    let events = lifecycle_events(&h).await;
    assert!(
        !events.iter().any(|e| e.2.contains("although")),
        "{events:#?}"
    );
}

/// `api` (routed, start-first) limits its memory to 64 MiB; `storage` sets no limit.
const LIMITED: &str = r#"
version = 1
name = "notes"

[services.api]
image = "docker.io/library/nginx:1.27"
limits = { memory = "64MiB" }

[[services.api.routes]]
host = "notes.example.com"
port = 8080

[services.storage]
image = "docker.io/library/redis:7"
"#;

/// LIMITED live, its containers created by the deployment (their `OOMKilled` seen false).
async fn limited() -> Harness {
    let h = harness().await;
    h.delegate("cpu memory pids\n");
    let live = h
        .finish(h.apply(LIMITED).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(live.state, S::Live, "{live:?}");
    h
}

/// The messages of the reconciler's restart events, oldest first.
async fn restart_messages(h: &Harness) -> Vec<String> {
    lifecycle_events(h)
        .await
        .into_iter()
        .filter(|(kind, ..)| *kind == EventKind::ContainerRestarted)
        .map(|(_, _, message, _)| message)
        .collect()
}

/// ADR-0059 §6: a 137 with `OOMKilled` is an OOM kill with the committed limit; the sticky
/// flag does not blame a later exit; an exit where the flag turned true since hoster last saw
/// it (a child OOM-killed) names it too, "(no memory limit)" without one. The restart record
/// keeps the last reason.
#[tokio::test]
async fn restarts_name_oom_kills_from_the_flag_hoster_last_saw() {
    let h = limited().await;
    let api = h.container("hoster-notes-api-g1").id;
    let storage = h.container("hoster-notes-storage-g1").id;
    let start = Instant::now();
    let at = |secs: u64| start + Duration::from_secs(secs);

    h.runtime.oom_kill(&api);
    // A child of storage is OOM-killed; storage exits 0 later.
    h.runtime.oom_kill_child(&storage);
    h.runtime
        .set_state(&storage, ContainerState::Exited(Some(0)));
    h.orchestrator.reconcile_at(at(0)).await.unwrap();
    let api_stats = service_status(&h.orchestrator, "api").await.restarts;
    assert_eq!(api_stats.restarts, 1);
    assert_eq!(
        api_stats.last_exit.map(|r| r.to_string()).as_deref(),
        Some("OOM-killed (limit 64MiB)")
    );

    // The flag stays true across the start; a later exit 1 is not an OOM.
    assert!(h.container("hoster-notes-api-g1").state.is_running());
    h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
    h.runtime
        .set_state(&storage, ContainerState::Exited(Some(0)));
    h.orchestrator.reconcile_at(at(1)).await.unwrap();
    // A 137 with the flag already true is still named: the exit is the evidence.
    h.runtime.oom_kill(&api);
    h.orchestrator.reconcile_at(at(3)).await.unwrap();

    assert_eq!(
        restart_messages(&h).await,
        [
            "Restarted notes/api: OOM-killed (limit 64MiB)",
            "Restarted notes/storage: exited 0 after an OOM kill (no memory limit)",
            "Restarted notes/api: exit 1",
            "Restarted notes/storage: exit 0",
            "Restarted notes/api: OOM-killed (limit 64MiB)",
        ]
    );
    let storage_stats = service_status(&h.orchestrator, "storage").await.restarts;
    assert_eq!(
        storage_stats.last_exit,
        Some(ExitReason::Exit(ContainerState::Exited(Some(0))))
    );
}

/// After a hoster restart the flags are gone: a container adopted with `OOMKilled` already
/// true has its OOM named only on a 137.
#[tokio::test]
async fn an_adopted_container_with_the_flag_true_is_named_only_on_a_137() {
    let h = limited().await;
    let api = h.container("hoster-notes-api-g1").id;
    h.runtime.oom_kill_child(&api);
    let orchestrator = restarted(&h, RestartPolicy::default());
    let report = orchestrator.recover().await.unwrap();
    assert!(report.adopted.contains(&"hoster-notes-api-g1".to_owned()));
    let start = Instant::now();

    h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
    orchestrator.reconcile_at(start).await.unwrap();
    h.runtime.oom_kill(&api);
    orchestrator
        .reconcile_at(start + Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        restart_messages(&h).await,
        [
            "Restarted notes/api: exit 1",
            "Restarted notes/api: OOM-killed (limit 64MiB)",
        ]
    );
}

/// A start that fails (a controller undelegated after apply: HTTP 500, the container left
/// `created`) is recorded as the reason; it writes no restart event.
#[tokio::test]
async fn a_failed_restart_records_the_start_error_as_the_reason() {
    let h = limited().await;
    let api = h.container("hoster-notes-api-g1").id;
    let refused = RuntimeError::Api {
        operation: Operation::Start,
        message: "controller `memory` is not available".into(),
    };
    h.runtime.fail_always(Operation::Start, refused.clone());
    h.runtime.set_state(&api, ContainerState::Created);
    let report = h.orchestrator.reconcile_at(Instant::now()).await.unwrap();
    assert_eq!(report.problems.len(), 1, "{report:?}");

    let stats = service_status(&h.orchestrator, "api").await.restarts;
    assert_eq!(stats.restarts, 1);
    assert_eq!(
        stats.last_exit,
        Some(ExitReason::StartFailed(refused.to_string()))
    );
    assert!(restart_messages(&h).await.is_empty());
}

#[tokio::test]
async fn containers_hoster_starts_are_followed_until_removed() {
    use crate::logs::LogSink;
    use crate::runtime::{LogLine, LogStream};

    let h = harness().await;
    let (sink, mut tap) = LogSink::tapped(1_024);
    let followers = h.orchestrator.connect_logs(sink, CancellationToken::new());
    h.bootstrap().await;
    let followed = |expected: Vec<ContainerId>| {
        let followers = followers.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                while followers.followed() != expected {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("followed {:?}", followers.followed()));
        }
    };
    // The live generations; the replaced ones' followers ended with their containers.
    let mut live: Vec<_> = ["api-g2", "storage-g1", "web-g2"]
        .map(|c| h.container(&format!("hoster-notes-{c}")).id)
        .into();
    live.sort();
    followed(live.clone()).await;

    let api = h.container("hoster-notes-api-g2").id;
    let print = |text: &str| {
        h.runtime.push_log(
            &api,
            LogLine {
                stream: LogStream::Stdout,
                time: Some(std::time::SystemTime::now()),
                text: text.to_owned(),
            },
        );
    };
    let mut next = async || {
        tokio::time::timeout(Duration::from_secs(5), tap.next())
            .await
            .expect("a record arrives")
            .unwrap()
            .0
    };
    print("serving");
    let record = next().await;
    assert_eq!(
        (
            record.stack.as_deref(),
            record.service.as_str(),
            record.generation
        ),
        (Some("notes"), "api", Some(2))
    );
    assert_eq!(record.message, "serving");

    // A crash ends the follower; the reconciler's restart starts a new one, which reads on.
    h.runtime.set_state(&api, ContainerState::Exited(Some(1)));
    followed(live.iter().filter(|id| **id != api).cloned().collect()).await;
    print("crashed");
    let report = h.orchestrator.reconcile_at(Instant::now()).await.unwrap();
    assert_eq!(report.started, ["hoster-notes-api-g2"]);
    followed(live).await;
    assert_eq!(next().await.message, "crashed");

    h.orchestrator
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    followed(Vec::new()).await;
}
