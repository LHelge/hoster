//! Image retention, image garbage collection and rollback (ADR-0041).

use super::gc::Retention;
use super::tests::{Harness, NOTES, WITH_SECRETS, digest, harness};
use super::*;
use crate::runtime::{ContainerRuntime, Operation};
use crate::store::{ImageUse, ImageUseKind};

use DeploymentState as S;

fn image_use(service: &str, d: u8, kind: ImageUseKind) -> ImageUse {
    ImageUse {
        stack: "notes".into(),
        service: service.into(),
        digest: digest(d),
        kind,
    }
}

fn live(service: &str, d: u8, id: i64) -> ImageUse {
    image_use(service, d, ImageUseKind::Live(DeploymentId::from(id)))
}

#[test]
fn retention_keeps_the_current_digest_and_the_last_three_successful_ones() {
    let uses = [
        image_use("api", 6, ImageUseKind::Current),
        // Newest first, as the store lists them, but the order must not matter.
        live("api", 1, 1),
        live("api", 6, 7),
        live("api", 5, 6),
        live("api", 4, 5),
        // A digest deployed twice counts once.
        live("api", 4, 4),
        live("api", 3, 3),
        live("api", 2, 2),
    ];
    let retention = Retention::new(3, &uses);
    for kept in [6, 5, 4, 3] {
        assert!(retention.keeps(&digest(kept)), "digest {kept}");
    }
    for pruned in [2, 1] {
        assert!(!retention.keeps(&digest(pruned)), "digest {pruned}");
    }
}

#[test]
fn retention_is_per_service_and_keeps_in_flight_targets() {
    let uses = [
        image_use("api", 4, ImageUseKind::Current),
        live("api", 4, 4),
        live("api", 3, 3),
        live("api", 2, 2),
        live("api", 1, 1),
        // web's history is its own: one successful digest besides the current one.
        image_use("web", 0x20, ImageUseKind::Current),
        live("web", 0x20, 4),
        live("web", 0x10, 2),
        // A deployment in flight keeps what it is about to run.
        image_use("api", 0x99, ImageUseKind::InFlight),
    ];
    let retention = Retention::new(1, &uses);
    for kept in [4, 3, 0x20, 0x10, 0x99] {
        assert!(retention.keeps(&digest(kept)), "digest {kept:#x}");
    }
    for pruned in [2, 1] {
        assert!(!retention.keeps(&digest(pruned)), "digest {pruned}");
    }
    // Nothing used, nothing kept.
    assert!(!Retention::new(3, &[]).keeps(&digest(4)));
}

fn api_image(d: u8) -> String {
    format!("ghcr.io/acme/notes-api@{}", digest(d))
}

/// NOTES applied, then api deployed at digests 1 to 5 (web at 0x21): five successful api
/// digests, the newest current. Also returns the deployment of api digest 1.
async fn five_api_deploys() -> (Harness, DeployOutcome) {
    let h = harness().await;
    let applied = h.apply(NOTES).await;
    h.finish(applied.deployment.as_ref().unwrap()).await;
    let first = h.deploy(&[("api", 1), ("web", 0x21)]).await;
    assert_eq!(h.finish(&first).await.state, S::Live);
    for d in 2..=5 {
        let next = h.deploy(&[("api", d)]).await;
        assert_eq!(h.finish(&next).await.state, S::Live);
    }
    (h, first)
}

#[tokio::test]
async fn pulls_are_recorded_and_gc_prunes_only_images_outside_every_retention_set() {
    let (h, _) = five_api_deploys().await;
    let recorded: Vec<_> = h
        .store
        .recorded_images()
        .await
        .unwrap()
        .into_iter()
        .map(|r| format!("{}@{}", r.repository, r.digest))
        .collect();
    assert_eq!(recorded.len(), 7, "{recorded:?}");
    assert!(recorded.contains(&api_image(1)));

    // An image hoster did not pull: present in podman, absent from the table.
    let foreign: PinnedImage = format!("docker.io/library/postgres@{}", digest(0x77))
        .parse()
        .unwrap();
    h.runtime.pull_image(&foreign).await.unwrap();
    // A recorded image that is already gone from podman is just forgotten.
    h.store
        .record_image("ghcr.io/acme/gone", &digest(0x66))
        .await
        .unwrap();

    let report = h.orchestrator.collect_garbage().await.unwrap();
    assert!(!report.deferred);
    assert!(report.failed.is_empty(), "{report:?}");
    let mut removed = report.removed.clone();
    removed.sort();
    assert_eq!(
        removed,
        [format!("ghcr.io/acme/gone@{}", digest(0x66)), api_image(1)]
    );
    // api: current 5 plus 4, 3, 2; web and storage: their current digests.
    assert_eq!(report.retained, 6);

    assert!(!h.runtime.images().contains(&api_image(1)));
    for d in 2..=5 {
        assert!(h.runtime.images().contains(&api_image(d)), "digest {d}");
    }
    assert!(h.runtime.has_image(&foreign), "foreign image untouched");
    let left = h.store.recorded_images().await.unwrap();
    assert_eq!(left.len(), 6);
    assert!(
        left.iter()
            .all(|r| r.digest != digest(1) || r.repository.ends_with("web"))
    );

    // Nothing more to prune on the next pass.
    let again = h.orchestrator.collect_garbage().await.unwrap();
    assert!(again.removed.is_empty(), "{again:?}");
    assert!(h.runtime.has_image(&foreign));
}

#[tokio::test]
async fn a_failed_removal_keeps_its_record_and_is_retried_next_pass() {
    let (h, _) = five_api_deploys().await;
    h.runtime.fail_next(
        Operation::RemoveImage,
        RuntimeError::InUse {
            kind: ObjectKind::Image,
            name: api_image(1),
        },
    );
    let report = h.orchestrator.collect_garbage().await.unwrap();
    assert!(report.removed.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, api_image(1));
    assert!(h.runtime.images().contains(&api_image(1)));
    assert!(
        h.store
            .recorded_images()
            .await
            .unwrap()
            .iter()
            .any(|r| r.digest == digest(1) && r.repository.ends_with("notes-api"))
    );

    let retried = h.orchestrator.collect_garbage().await.unwrap();
    assert_eq!(retried.removed, [api_image(1)]);
    assert!(!h.runtime.images().contains(&api_image(1)));
}

#[tokio::test]
async fn an_image_a_container_uses_is_never_removed() {
    let (h, _) = five_api_deploys().await;
    // Something outside hoster's bookkeeping runs on api digest 1: podman refuses the removal.
    let spec = h.spec_of("hoster-notes-api-g6");
    let mut stray = spec.clone();
    stray.name = "stray".into();
    stray.image = api_image(1).parse().unwrap();
    h.runtime.create_container(&stray).await.unwrap();

    let report = h.orchestrator.collect_garbage().await.unwrap();
    assert!(report.removed.is_empty(), "{report:?}");
    assert_eq!(report.failed.len(), 1);
    assert!(report.failed[0].1.contains("in use"), "{report:?}");
    assert!(h.runtime.images().contains(&api_image(1)));
}

#[tokio::test]
async fn gc_defers_while_a_deployment_is_active() {
    let (h, _) = five_api_deploys().await;
    let hold = h.runtime.hold_pulls(&digest(9));
    let pending = h.deploy(&[("api", 9)]).await;
    hold.reached().await;
    let report = h.orchestrator.collect_garbage().await.unwrap();
    assert!(report.deferred);
    assert!(report.removed.is_empty());
    hold.release();
    assert_eq!(h.finish(&pending).await.state, S::Live);
    let report = h.orchestrator.collect_garbage().await.unwrap();
    assert!(!report.deferred);
    // api 9 is current now; 5, 4, 3 stay, 2 and 1 go.
    let mut removed = report.removed;
    removed.sort();
    assert_eq!(removed, [api_image(1), api_image(2)]);
}

#[tokio::test]
async fn a_rollback_started_during_a_removal_waits_for_it_and_pulls_again() {
    let (h, first) = five_api_deploys().await;

    // GC is removing api digest 1, which no retention set keeps any more...
    let hold = h.runtime.hold_image_removals(&digest(1));
    let gc = tokio::spawn({
        let orchestrator = Arc::clone(&h.orchestrator);
        async move { orchestrator.collect_garbage().await }
    });
    hold.reached().await;
    // ...when a rollback to it starts. It must not find the image present now and lose it to
    // the removal before its container is created.
    let outcome = h
        .orchestrator
        .rollback("notes", first.deployment)
        .await
        .unwrap();
    let pulling = async {
        while h
            .store
            .deployment(outcome.deployment)
            .await
            .unwrap()
            .is_none_or(|d| d.state != S::Pulling)
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), pulling)
        .await
        .expect("the rollback reaches Pulling");
    tokio::time::sleep(Duration::from_millis(20)).await;
    hold.release();

    let report = gc.await.unwrap().unwrap();
    assert_eq!(report.removed, [api_image(1)], "{report:?}");
    let rolled = h.finish(&outcome).await;
    assert_eq!(rolled.state, S::Live, "{rolled:?}");
    // It pulled again after the removal, and recorded the pull.
    let calls = h.runtime.calls();
    let removed = calls
        .iter()
        .rposition(|op| *op == Operation::RemoveImage)
        .unwrap();
    let pulled = calls
        .iter()
        .rposition(|op| *op == Operation::PullImage)
        .unwrap();
    assert!(pulled > removed, "{calls:?}");
    assert!(h.runtime.images().contains(&api_image(1)));
    assert!(
        h.store
            .recorded_images()
            .await
            .unwrap()
            .iter()
            .any(|r| r.digest == digest(1) && r.repository.ends_with("notes-api"))
    );
}

#[tokio::test]
async fn a_removed_stack_keeps_no_images() {
    let (h, _) = five_api_deploys().await;
    h.orchestrator
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    let report = h.orchestrator.collect_garbage().await.unwrap();
    assert_eq!(report.retained, 0);
    assert_eq!(report.removed.len(), 7, "{report:?}");
    assert!(h.store.recorded_images().await.unwrap().is_empty());
}

#[tokio::test]
async fn rollback_uses_the_old_digests_and_revision_but_current_secret_values() {
    let h = harness().await;
    h.set_secret("notes/database-url", "postgres://one").await;
    h.apply(WITH_SECRETS).await;
    let good = h.deploy(&[("api", 1), ("multi", 1), ("worker", 1)]).await;
    assert_eq!(h.finish(&good).await.state, S::Live);
    let newer = h.deploy(&[("api", 2)]).await;
    assert_eq!(h.finish(&newer).await.state, S::Live);
    let changed = h
        .orchestrator
        .set_secret("notes", "database-url", &"postgres://two".into())
        .await
        .unwrap();
    assert_eq!(
        h.finish(changed.deployment.as_ref().unwrap()).await.state,
        S::Live
    );

    let outcome = h
        .orchestrator
        .rollback("notes", good.deployment)
        .await
        .unwrap();
    let rolled = h.finish(&outcome).await;
    assert_eq!(rolled.state, S::Live, "{rolled:?}");
    assert_eq!(rolled.source, DeploymentSource::Rollback);
    assert_eq!(rolled.target["api"], digest(1));
    let api = h
        .store
        .services("notes")
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.service == "api")
        .unwrap();
    assert_eq!(api.digest, digest(1));
    let spec = h.spec_of(&format!("hoster-notes-api-g{}", api.generation));
    assert_eq!(spec.image.to_string(), api_image(1));
    // Old secret values are not retained: the rollback runs with the current one.
    assert_eq!(spec.secrets["DATABASE_URL"].expose(), "postgres://two");

    // The rollback is an ordinary successful deployment: GC keeps what it runs.
    let report = h.orchestrator.collect_garbage().await.unwrap();
    assert!(report.removed.is_empty(), "{report:?}");
    assert!(h.runtime.images().contains(&api_image(1)));
}
