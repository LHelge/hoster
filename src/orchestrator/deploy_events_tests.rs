//! The deploy events of the event log (ADR-0055 §2): one line when a deployment is accepted
//! and one for its outcome, written by the orchestrator against [`FakeRuntime`] and a real
//! (temporary) store.

use std::time::Duration;

use super::tests::{NOTES, caller, digest, harness, harness_with, hooked, provenance, webhook};
use super::*;
use crate::runtime::{ObjectKind, Operation};
use crate::store::{Event, EventKind, EventQuery, Severity, ShortDigest};

use DeploymentState as S;

impl tests::Harness {
    /// The deploy events of deployment `id`, oldest first.
    async fn deploy_events(&self, id: DeploymentId) -> Vec<Event> {
        let query = EventQuery {
            kinds: [
                EventKind::DeployStarted,
                EventKind::DeploySucceeded,
                EventKind::DeployFailed,
                EventKind::DeployRolledBack,
            ]
            .into(),
            limit: EventQuery::MAX,
            ..EventQuery::default()
        };
        let subject = format!("/stacks/notes/deployments/{id}");
        let mut events: Vec<Event> = self
            .store
            .event_page(&query)
            .await
            .unwrap()
            .events
            .into_iter()
            .filter(|e| e.subject.as_deref() == Some(subject.as_str()))
            .collect();
        events.reverse();
        events
    }

    /// The kinds and severities of deployment `id`'s deploy events, oldest first.
    async fn deploy_outline(&self, id: DeploymentId) -> Vec<(EventKind, Severity)> {
        self.deploy_events(id)
            .await
            .into_iter()
            .map(|e| (e.kind, e.severity))
            .collect()
    }

    /// Waits until deployment `id` is in `state`.
    async fn waits_for(&self, id: DeploymentId, state: DeploymentState) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.store.deployment(id).await.unwrap().unwrap().state != state {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("deployment never reached {state}"));
    }
}

#[tokio::test]
async fn a_successful_deploy_writes_started_and_succeeded_and_announces_both() {
    let h = harness().await;
    h.bootstrap().await;
    let mut ui = h.orchestrator.subscribe();
    let outcome = h.deploy(&[("api", 2)]).await;
    let deployment = h.finish(&outcome).await;
    assert_eq!(deployment.state, S::Live);

    let events = h.deploy_events(deployment.id).await;
    let [started, succeeded] = events.as_slice() else {
        panic!("{events:#?}");
    };
    let subject = format!("/stacks/notes/deployments/{}", deployment.id);
    assert_eq!(started.kind, EventKind::DeployStarted);
    assert_eq!(started.severity, Severity::Info);
    assert_eq!(started.stack.as_deref(), Some("notes"));
    assert_eq!(started.subject.as_deref(), Some(subject.as_str()));
    assert_eq!(started.message, "Deploying notes");
    // Only the service the deployment changes, with its short digest; no commit is known.
    assert_eq!(
        started.meta.as_deref(),
        Some(format!("webhook · api {}", ShortDigest(&digest(2))).as_str())
    );
    assert_eq!(succeeded.kind, EventKind::DeploySucceeded);
    assert_eq!(succeeded.severity, Severity::Success);
    assert_eq!(succeeded.subject.as_deref(), Some(subject.as_str()));
    assert!(
        succeeded.message.starts_with("Deployed notes in "),
        "{succeeded:?}"
    );
    assert_eq!(succeeded.meta.as_deref(), Some("webhook"));

    let mut announced = Vec::new();
    while let Ok(event) = ui.try_recv() {
        if let UiEvent::Event(event) = event {
            announced.push(event);
        }
    }
    assert_eq!(announced, events);
}

#[tokio::test]
async fn a_webhook_deploy_carries_its_commit() {
    let h = harness().await;
    h.finish(h.apply(&hooked()).await.deployment.as_ref().unwrap())
        .await;
    let outcome = h
        .orchestrator
        .deploy_webhook(webhook(
            "notes",
            &[("api", 1)],
            caller(42, "refs/tags/v1.4.2"),
        ))
        .await
        .unwrap();
    let deployment = h.finish(&outcome).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    // Only the short sha is written, never the full 40 digits.
    let sha = &provenance().sha[..7];
    let events = h.deploy_events(deployment.id).await;
    let metas: Vec<_> = events.iter().map(|e| e.meta.clone().unwrap()).collect();
    assert_eq!(
        metas,
        [
            format!("webhook · {sha} · api {}", ShortDigest(&digest(1))),
            format!("webhook · {sha}"),
        ]
    );
}

#[tokio::test]
async fn a_failed_deploy_writes_the_error() {
    let h = harness().await;
    h.bootstrap().await;
    h.runtime.fail_next(
        Operation::PullImage,
        RuntimeError::NotFound {
            kind: ObjectKind::Image,
            name: "ghcr.io/acme/notes-api".into(),
        },
    );
    let failed = h.finish(&h.deploy(&[("api", 2)]).await).await;
    assert_eq!(failed.state, S::Failed);
    assert_eq!(
        h.deploy_outline(failed.id).await,
        [
            (EventKind::DeployStarted, Severity::Info),
            (EventKind::DeployFailed, Severity::Error)
        ]
    );
    let events = h.deploy_events(failed.id).await;
    let error = failed.error.unwrap();
    assert_eq!(
        events[1].message,
        format!("Deployment of notes failed: {error}")
    );
}

#[tokio::test]
async fn a_rejected_deploy_is_a_failure() {
    let h = harness().await;
    h.apply(tests::WITH_SECRETS).await;
    let rejected = h.finish(&h.deploy(&[("api", 1)]).await).await;
    assert_eq!(rejected.state, S::Rejected, "{rejected:?}");
    assert_eq!(
        h.deploy_outline(rejected.id).await,
        [
            (EventKind::DeployStarted, Severity::Info),
            (EventKind::DeployFailed, Severity::Error)
        ]
    );
}

#[tokio::test]
async fn a_deploy_whose_cleanup_fails_ends_with_a_warning_naming_the_problem() {
    let h = harness().await;
    h.bootstrap().await;
    h.runtime.fail_next(
        Operation::Remove,
        RuntimeError::NotFound {
            kind: ObjectKind::Container,
            name: "notes-api".into(),
        },
    );
    let warned = h.finish(&h.deploy(&[("api", 2)]).await).await;
    assert_eq!(warned.state, S::LiveWithWarnings, "{warned:?}");
    assert_eq!(
        h.deploy_outline(warned.id).await,
        [
            (EventKind::DeployStarted, Severity::Info),
            (EventKind::DeploySucceeded, Severity::Success),
            (EventKind::DeploySucceeded, Severity::Warning)
        ]
    );
    let events = h.deploy_events(warned.id).await;
    let warning = &events[2].message;
    assert!(
        warning.starts_with("Deployment of notes live with warnings: cannot remove "),
        "{warning}"
    );
    assert_eq!(events[2].meta.as_deref(), Some("webhook"));
}

#[tokio::test]
async fn a_cancelled_deploy_is_a_warning() {
    let h = harness().await;
    h.bootstrap().await;
    h.prober.hold(8080);
    let outcome = h.deploy(&[("api", 2)]).await;
    h.waits_for(outcome.deployment, S::HealthChecking).await;
    h.prober.probed(8080).await;
    h.orchestrator
        .cancel("notes", outcome.deployment)
        .await
        .unwrap();
    let cancelled = h.finish(&outcome).await;
    assert_eq!(cancelled.error.as_deref(), Some(CANCELLED));
    assert_eq!(
        h.deploy_outline(cancelled.id).await,
        [
            (EventKind::DeployStarted, Severity::Info),
            (EventKind::DeployFailed, Severity::Warning)
        ]
    );
    let events = h.deploy_events(cancelled.id).await;
    assert_eq!(events[1].message, "Deployment of notes cancelled");
}

#[tokio::test]
async fn removing_the_stack_mid_deploy_is_a_warning_and_a_queued_deploy_writes_nothing() {
    let h = harness_with(Duration::from_secs(60)).await;
    let active = h.apply(NOTES).await.deployment.unwrap();
    let queued = h.deploy(&[("api", 1)]).await;
    assert_eq!(queued.state, S::Queued);
    tokio::time::timeout(
        Duration::from_secs(10),
        h.orchestrator.remove(RemoveStack::keeping_volumes("notes")),
    )
    .await
    .expect("removal does not wait for the health check")
    .unwrap();
    let outline = h.deploy_outline(active.deployment).await;
    assert_eq!(
        outline,
        [
            (EventKind::DeployStarted, Severity::Info),
            (EventKind::DeployFailed, Severity::Warning)
        ]
    );
    let events = h.deploy_events(active.deployment).await;
    assert_eq!(
        events[1].message,
        "Deployment of notes ended: stack removed"
    );
    // The queued deployment was superseded before it was ever accepted.
    assert!(h.deploy_events(queued.deployment).await.is_empty());
}

#[tokio::test]
async fn a_noop_deploy_writes_no_event() {
    let h = harness().await;
    h.bootstrap().await;
    let outcome = h.deploy(&[("api", 1)]).await;
    assert_eq!(outcome.state, S::Noop);
    assert!(h.deploy_events(outcome.deployment).await.is_empty());
}

#[tokio::test]
async fn a_rollback_reads_as_one_with_its_target() {
    let h = harness().await;
    h.bootstrap().await;
    let good = h.store.deployments("notes", 1).await.unwrap().remove(0);
    h.finish(&h.deploy(&[("api", 2)]).await).await;
    let outcome = h.orchestrator.rollback("notes", good.id).await.unwrap();
    let rolled = h.finish(&outcome).await;
    assert_eq!(rolled.state, S::Live);

    let events = h.deploy_events(rolled.id).await;
    let [started, rolled_back] = events.as_slice() else {
        panic!("{events:#?}");
    };
    assert_eq!(started.kind, EventKind::DeployStarted);
    assert_eq!(
        started.message,
        format!("Rolling back notes to deployment #{}", good.id)
    );
    assert!(
        started
            .meta
            .as_deref()
            .unwrap()
            .starts_with("rollback · api "),
        "{started:?}"
    );
    assert_eq!(rolled_back.kind, EventKind::DeployRolledBack);
    assert_eq!(rolled_back.severity, Severity::Success);
    assert!(
        rolled_back
            .message
            .starts_with(&format!("Rolled back notes to deployment #{} in ", good.id)),
        "{rolled_back:?}"
    );
}
