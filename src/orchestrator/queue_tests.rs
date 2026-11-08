//! Deploy concurrency (ADR-0040): the per-stack queue that keeps only the newest target, the
//! idempotency check a queued target gets when its turn comes, cancelling an active deployment
//! before `Switching`, and the global limit on concurrent pulls.
//!
//! Latencies are controlled through [`FakeRuntime`] pull holds and [`FakeProber`] held ports,
//! so every interleaving is deterministic.

use std::sync::Arc;
use std::time::Duration;

use super::tests::{Harness, NOTES, digest, harness};
use super::*;
use crate::routing::{Target as RouteTarget, Upstream};
use crate::runtime::ContainerState;

use DeploymentState as S;

/// A stack of one unrouted third-party service; `name` also picks its image.
fn worker(name: &str) -> String {
    format!(
        "version = 1\nname = \"{name}\"\n\n[services.worker]\nimage = \"docker.io/acme/{name}-worker:1\"\n"
    )
}

/// NOTES with storage's config changed, so a deploy replaces it (stop-first).
fn storage_changed() -> String {
    NOTES.replace(
        "data = \"/data\"",
        "data = \"/data\"\n\n[services.storage.env]\nA = \"1\"",
    )
}

impl Harness {
    /// Waits until deployment `id` is in `state`.
    async fn waits_in(&self, id: DeploymentId, state: DeploymentState) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let deployment = self.store.deployment(id).await.unwrap().unwrap();
                if deployment.state == state {
                    return;
                }
                assert!(
                    !deployment.state.is_terminal(),
                    "ended in {} before reaching {state}",
                    deployment.state
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("deployment never reached {state}"));
    }

    async fn api_digest(&self) -> String {
        let pointers = self.store.services("notes").await.unwrap();
        pointers
            .into_iter()
            .find(|p| p.service == "api")
            .unwrap()
            .digest
    }

    fn api_upstream(&self) -> Arc<Upstream> {
        match &self
            .routes
            .load()
            .lookup("notes.example.com", "/api/notes")
            .expect("api is routed")
            .target
        {
            RouteTarget::Loopback(upstream) => Arc::clone(upstream),
            other => panic!("api routes to {other:?}"),
        }
    }
}

#[tokio::test]
async fn a_queued_target_that_runs_by_its_turn_becomes_a_noop() {
    let h = harness().await;
    h.bootstrap().await;
    let hold = h.runtime.hold_pulls("notes-api");
    let first = h.deploy(&[("api", 2)]).await;
    hold.reached().await;
    // Not running yet when it is submitted, so it queues rather than being a `Noop`.
    let again = h.deploy(&[("api", 2)]).await;
    assert_eq!(again.state, S::Queued);

    hold.release();
    assert_eq!(h.finish(&first).await.state, S::Live);
    let again = h.finish(&again).await;
    assert_eq!(again.state, S::Noop, "{again:?}");
    assert_eq!(
        h.states(again.id).await,
        [S::Received, S::Queued, S::Received, S::Noop]
    );
    assert_eq!(h.api_digest().await, digest(2));
}

#[tokio::test]
async fn only_the_newest_pending_target_is_kept_and_older_ones_are_superseded() {
    let h = harness().await;
    h.bootstrap().await;
    let hold = h.runtime.hold_pulls("notes-api");
    let active = h.deploy(&[("api", 2)]).await;
    hold.reached().await;
    let mut queued = Vec::new();
    for d in [3, 4, 5] {
        queued.push(h.deploy(&[("api", d)]).await);
    }
    assert!(queued.iter().all(|q| q.state == S::Queued), "{queued:?}");
    for older in &queued[..2] {
        let older = h.finish(older).await;
        assert_eq!(older.state, S::Superseded);
        assert_eq!(
            h.states(older.id).await,
            [S::Received, S::Queued, S::Superseded]
        );
    }
    // Superseded deployments never held the slot: the active one is still the first.
    let status = h.orchestrator.status().await.unwrap();
    assert_eq!(status[0].active.map(|(id, _)| id), Some(active.deployment));
    assert_eq!(status[0].queued, Some(queued[2].deployment));

    hold.release();
    assert_eq!(h.finish(&active).await.state, S::Live);
    assert_eq!(h.finish(&queued[2]).await.state, S::Live);
    assert_eq!(h.api_digest().await, digest(5));
}

#[tokio::test]
async fn cancelling_while_pulling_aborts_and_the_queued_target_goes_next() {
    let h = harness().await;
    h.bootstrap().await;
    let ports_before = h.store.port_allocations().await.unwrap();
    let hold = h.runtime.hold_pulls("notes-api");
    let active = h.deploy(&[("api", 2)]).await;
    hold.reached().await;
    let queued = h.deploy(&[("web", 2)]).await;
    assert_eq!(queued.state, S::Queued);

    h.orchestrator
        .cancel("notes", active.deployment)
        .await
        .unwrap();
    // The pull is still held: the cancel wakes the deploy task from it.
    let cancelled = h.finish(&active).await;
    assert_eq!(cancelled.state, S::Failed, "{cancelled:?}");
    assert_eq!(cancelled.error.as_deref(), Some("cancelled"));
    assert_eq!(
        h.states(cancelled.id).await,
        [
            S::Received,
            S::Validating,
            S::Pulling,
            S::Aborting,
            S::Failed
        ]
    );
    assert_eq!(h.api_digest().await, digest(1));

    // The queued deployment is not affected by the cancel and starts next.
    let next = h.finish(&queued).await;
    assert_eq!(next.state, S::Live, "{next:?}");
    assert_eq!(h.api_digest().await, digest(1));
    assert_eq!(
        h.store.port_allocations().await.unwrap().len(),
        ports_before.len(),
        "the cancelled deployment's ports are released"
    );
    hold.release();
}

#[tokio::test]
async fn cancelling_while_health_checking_removes_the_new_containers() {
    let h = harness().await;
    h.bootstrap().await;
    let before = h.container_names();
    let old_api = h.api_upstream();
    h.prober.hold(8080);
    let outcome = h.deploy(&[("api", 2)]).await;
    h.waits_in(outcome.deployment, S::HealthChecking).await;
    h.prober.probed(8080).await;
    assert!(
        h.container_names()
            .contains(&"hoster-notes-api-g3".to_owned())
    );

    h.orchestrator
        .cancel("notes", outcome.deployment)
        .await
        .unwrap();
    let cancelled = h.finish(&outcome).await;
    assert_eq!(cancelled.state, S::Failed, "{cancelled:?}");
    assert_eq!(cancelled.error.as_deref(), Some("cancelled"));
    assert!(h.states(cancelled.id).await.contains(&S::Aborting));
    assert_eq!(h.container_names(), before);
    assert_eq!(h.api_upstream().addr, old_api.addr, "routes never moved");
}

#[tokio::test]
async fn cancelling_while_replacing_restarts_the_stopped_stop_first_container() {
    let h = harness().await;
    h.bootstrap().await;
    h.prober.hold(9000);
    let applied = h.apply(&storage_changed()).await;
    let outcome = applied.deployment.unwrap();
    h.waits_in(outcome.deployment, S::Replacing).await;
    h.prober.probed(9000).await;
    assert_ne!(
        h.container("hoster-notes-storage-g1").state,
        ContainerState::Running
    );

    h.orchestrator
        .cancel("notes", outcome.deployment)
        .await
        .unwrap();
    let cancelled = h.finish(&outcome).await;
    assert_eq!(cancelled.state, S::Failed, "{cancelled:?}");
    assert_eq!(
        h.container("hoster-notes-storage-g1").state,
        ContainerState::Running,
        "the abort restarted the old storage container"
    );
    assert!(
        !h.container_names()
            .iter()
            .any(|n| n.contains("storage-g") && n != "hoster-notes-storage-g1"),
        "{:?}",
        h.container_names()
    );
    let storage = h.store.services("notes").await.unwrap();
    assert_eq!(
        storage
            .iter()
            .find(|p| p.service == "storage")
            .unwrap()
            .generation,
        1
    );
}

#[tokio::test]
async fn only_active_deployments_before_switching_can_be_cancelled() {
    let h = harness().await;
    h.bootstrap().await;

    // From `Switching` on, the deployment commits: an in-flight request keeps it in `Draining`.
    let in_flight = h.api_upstream().track();
    let draining = h.deploy(&[("api", 2)]).await;
    h.waits_in(draining.deployment, S::Draining).await;
    let hold = h.runtime.hold_pulls("notes-web");
    let queued = h.deploy(&[("web", 2)]).await;
    assert_eq!(queued.state, S::Queued);

    for (id, state) in [
        (draining.deployment, S::Draining),
        (queued.deployment, S::Queued),
    ] {
        match h.orchestrator.cancel("notes", id).await {
            Err(OrchestratorError::NotCancellable {
                id: refused,
                state: s,
            }) => {
                assert_eq!((refused, s), (id, state));
            }
            other => panic!("cancel of a {state} deployment: {other:?}"),
        }
    }
    drop(in_flight);
    assert_eq!(h.finish(&draining).await.state, S::Live);
    hold.reached().await;
    hold.release();
    let done = h.finish(&queued).await;
    assert_eq!(done.state, S::Live);

    // Over already, or of another stack.
    assert!(matches!(
        h.orchestrator.cancel("notes", done.id).await,
        Err(OrchestratorError::NotCancellable { state: S::Live, .. })
    ));
    assert!(matches!(
        h.orchestrator.cancel("blog", done.id).await,
        Err(OrchestratorError::UnknownDeployment(_))
    ));
}

#[tokio::test]
async fn at_most_two_pulls_run_at_once_over_every_stack() {
    let h = harness().await;
    let holds = [
        h.runtime.hold_pulls("one-worker"),
        h.runtime.hold_pulls("two-worker"),
    ];
    let one = h.apply(&worker("one")).await.deployment.unwrap();
    let two = h.apply(&worker("two")).await.deployment.unwrap();
    for hold in &holds {
        hold.reached().await;
    }

    // Two pulls are running: the next two stacks wait in `Pulling` for a permit, without
    // reaching podman.
    let three = h.apply(&worker("three")).await.deployment.unwrap();
    let four = h.apply(&worker("four")).await.deployment.unwrap();
    for waiting in [&three, &four] {
        h.waits_in(waiting.deployment, S::Pulling).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !h.runtime
            .calls()
            .contains(&crate::runtime::Operation::PullImage),
        "a pull reached podman past the limit"
    );

    // A cancel ends the wait for a permit too.
    h.orchestrator
        .cancel("four", four.deployment)
        .await
        .unwrap();
    let cancelled = h.finish(&four).await;
    assert_eq!(cancelled.state, S::Failed, "{cancelled:?}");
    assert_eq!(cancelled.error.as_deref(), Some("cancelled"));

    for hold in &holds {
        hold.release();
    }
    for deployment in [&one, &two, &three] {
        assert_eq!(h.finish(deployment).await.state, S::Live);
    }
}
