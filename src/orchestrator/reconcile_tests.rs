//! Reconciler scenarios (ADR-0028, architecture §5.4): the orchestrator against [`FakeRuntime`]
//! and a real (temporary) store, with a "restart" being a fresh orchestrator on both.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::tests::{Harness, NOTES, digest, harness};
use super::*;
use crate::config::PortRange;
use crate::runtime::{
    ContainerId, ContainerSpec, ContainerState, FakeRuntime, Labels, Operation, ServiceGeneration,
};
use crate::store::{NewDeployment, ServicePointer};

use DeploymentState as S;

fn name(s: &str) -> Name {
    s.parse().unwrap()
}

fn ports(store: &Store) -> PortAllocator {
    PortAllocator::new(store.clone(), PortRange::try_from(20000..=20015).unwrap())
}

/// hoster starting again on the same store and podman.
fn restarted(h: &Harness, restart: RestartPolicy) -> Arc<Orchestrator<FakeRuntime>> {
    Orchestrator::new(
        h.store.clone(),
        Arc::clone(&h.runtime),
        Arc::clone(&h.resolver) as Arc<dyn TagResolver>,
        ports(&h.store),
        h.secrets.clone(),
        Settings {
            settle: Duration::from_millis(20),
            settle_poll: Duration::from_millis(5),
            restart,
            ..Settings::default()
        },
        Arc::default(),
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
                h.store.commit(record.id, &[pointer], &[]).await.unwrap();
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
            crash_looping: true
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
