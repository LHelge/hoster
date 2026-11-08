//! The digest poller against the real orchestrator ([`FakeRuntime`], a temporary store): the
//! tracked services it reports, and deploys and rollbacks as the poller sees them.

use std::sync::atomic::Ordering;

use super::tests::{NOTES, digest, harness};
use super::*;
use crate::config::PollerConfig;
use crate::registry::Poller;

/// NOTES with `api` tracking `main`.
fn tracked_notes() -> String {
    NOTES.replace(
        "image = \"ghcr.io/acme/notes-api\"\n",
        "image = \"ghcr.io/acme/notes-api\"\ntrack = \"main\"\n",
    )
}

impl super::tests::Harness {
    fn poller(&self) -> Poller {
        Poller::new(
            Box::new(Arc::clone(&self.orchestrator)),
            Arc::clone(&self.resolver) as Arc<dyn TagResolver>,
            Arc::new(self.store.clone()),
            &PollerConfig::default(),
        )
    }

    /// Moves every tag upstream to `digest(n)`.
    fn release(&self, n: u8) {
        self.resolver.digest.store(n, Ordering::SeqCst);
    }

    /// The stack's newest deployment, once it has finished.
    async fn newest(&self) -> Deployment {
        let newest = self.store.deployments("notes", 1).await.unwrap().remove(0);
        tokio::time::timeout(Duration::from_secs(10), self.orchestrator.wait(newest.id))
            .await
            .expect("deployment finishes")
            .unwrap()
    }

    async fn running(&self, service: &str) -> String {
        self.store
            .services("notes")
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.service == service)
            .map(|p| p.digest)
            .unwrap_or_default()
    }
}

#[tokio::test]
async fn tracked_lists_services_that_set_track_with_their_running_digest() {
    let h = harness().await;
    let applied = h.apply(&tracked_notes()).await;
    h.finish(applied.deployment.as_ref().unwrap()).await;

    let tracked = h.orchestrator.tracked().await.unwrap();
    assert_eq!(
        tracked,
        vec![TrackedService {
            stack: "notes".into(),
            service: "api".into(),
            image: "ghcr.io/acme/notes-api:main".parse().unwrap(),
            running: None,
        }]
    );

    h.finish(&h.deploy(&[("api", 1), ("web", 1)]).await).await;
    let tracked = h.orchestrator.tracked().await.unwrap();
    assert_eq!(tracked[0].running, Some(digest(1).parse().unwrap()));
}

#[tokio::test]
async fn the_poller_bootstraps_follows_the_tag_and_never_undoes_a_rollback() {
    let h = harness().await;
    let applied = h.apply(&tracked_notes()).await;
    h.finish(applied.deployment.as_ref().unwrap()).await;
    let mut poller = h.poller();

    // First sighting on a service that never ran: deployed, from the poller.
    h.release(1);
    poller.poll().await.unwrap();
    let first = h.newest().await;
    assert_eq!(first.source, DeploymentSource::Poller);
    assert_eq!(first.state, DeploymentState::Live, "{first:?}");
    assert_eq!(h.running("api").await, digest(1));

    // Unchanged: nothing new.
    poller.poll().await.unwrap();
    assert_eq!(h.newest().await.id, first.id);

    // The tag moves.
    h.release(2);
    poller.poll().await.unwrap();
    let second = h.newest().await;
    assert_ne!(second.id, first.id);
    assert_eq!(second.source, DeploymentSource::Poller);
    assert_eq!(h.running("api").await, digest(2));

    // Rolled back to digest 1: the unchanged tag must not redeploy 2, not even after a restart.
    let rolled = h.orchestrator.rollback("notes", first.id).await.unwrap();
    let rolled = h.finish(&rolled).await;
    assert_eq!(rolled.state, DeploymentState::Live, "{rolled:?}");
    poller.poll().await.unwrap();
    h.poller().poll().await.unwrap();
    assert_eq!(h.newest().await.id, rolled.id);
    assert_eq!(h.running("api").await, digest(1));

    // The next release moves the tag again.
    h.release(3);
    poller.poll().await.unwrap();
    let third = h.newest().await;
    assert_eq!(third.source, DeploymentSource::Poller);
    assert_eq!(h.running("api").await, digest(3));
}

#[tokio::test]
async fn first_sighting_of_a_service_that_runs_only_records() {
    let h = harness().await;
    let applied = h.apply(&tracked_notes()).await;
    h.finish(applied.deployment.as_ref().unwrap()).await;
    let webhook = h.finish(&h.deploy(&[("api", 1), ("web", 1)]).await).await;

    h.release(2);
    h.poller().poll().await.unwrap();

    assert_eq!(h.newest().await.id, webhook.id);
    assert_eq!(h.running("api").await, digest(1));
}
