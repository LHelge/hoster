//! One-click updates of third-party images (ADR-0061 §9).

use std::sync::atomic::Ordering;

use super::tests::{Harness, digest, harness};
use super::*;
use crate::store::{Candidate, ImageCheck};

use DeploymentState as S;

/// immich's server and machine learning share a release; db pins its digest in the file.
fn photos() -> String {
    format!(
        r#"# Photos, after the immich compose file.
version = 1
name = "photos"

[services.server]
image = "ghcr.io/immich-app/immich-server:v2.3.1"   # keep in step with ml
depends_on = ["db"]

[services.ml]
image = "ghcr.io/immich-app/immich-machine-learning:v2.3.1"

[services.db]
image = "docker.io/library/postgres:16.4@{}"
update = "stop-first"
"#,
        digest(0x11)
    )
}

impl Harness {
    /// Records that the checker found `found` for `service` of the active revision of photos.
    async fn found(&self, service: &str, found: &[(UpdateKind, &str)]) {
        let revision = self.store.latest_revision("photos").await.unwrap().unwrap();
        let pins = self.store.revision_images(revision.id).await.unwrap();
        let stack: Stack = revision.toml.parse().unwrap();
        let image = stack.services[&service.parse::<Name>().unwrap()]
            .image
            .clone();
        let check = ImageCheck {
            stack: "photos".into(),
            service: service.into(),
            image,
            digest: pins[service].parse().unwrap(),
        };
        let candidates: Vec<Candidate> = found
            .iter()
            .map(|(kind, tag)| Candidate {
                kind: *kind,
                tag: (*tag).into(),
                digest: (*kind == UpdateKind::Digest).then(|| digest(0xcc).parse().unwrap()),
            })
            .collect();
        self.store
            .record_image_check(&check, &candidates)
            .await
            .unwrap();
    }
}

fn choices(choices: &[&str]) -> Vec<UpdateChoice> {
    choices.iter().map(|c| c.parse().unwrap()).collect()
}

#[tokio::test]
async fn a_group_moves_together_and_the_file_keeps_its_comments() {
    let h = harness().await;
    let applied = h.apply(&photos()).await;
    h.finish(applied.deployment.as_ref().unwrap()).await;
    h.found(
        "server",
        &[(UpdateKind::Minor, "v2.4.0"), (UpdateKind::Major, "v3.0.0")],
    )
    .await;
    h.found("ml", &[(UpdateKind::Minor, "v2.4.0")]).await;
    h.found("db", &[(UpdateKind::Minor, "16.6")]).await;

    let updates = h.orchestrator.stack_updates("photos").await.unwrap();
    let offers: Vec<(String, UpdateKind, &str)> = updates
        .offers
        .iter()
        .map(|o| (o.services.join("+"), o.kind, o.to.as_str()))
        .collect();
    // server's major has no match in ml, so it is not offered for the group.
    assert_eq!(
        offers,
        [
            ("db".into(), UpdateKind::Minor, "16.6"),
            ("ml+server".into(), UpdateKind::Minor, "v2.4.0"),
        ]
    );
    let all = h.orchestrator.all_updates().await.unwrap();
    assert_eq!(all.keys().collect::<Vec<_>>(), ["photos"]);

    // Choosing one member previews the whole group. The new tag has a new digest upstream.
    h.resolver.digest.store(0xdd, Ordering::SeqCst);
    let update = h
        .orchestrator
        .update_preview("photos", &choices(&["server:minor"]))
        .await
        .unwrap();
    assert_eq!(update.revision.revision, 1);
    assert!(update.file.contains("immich-machine-learning:v2.4.0\""));
    let mut changed = update.preview.changed.clone();
    changed.sort();
    assert_eq!(changed, ["ml", "server"]);

    // A stale confirmation is refused.
    assert!(matches!(
        h.orchestrator
            .update("photos", &choices(&["server:minor"]), 7)
            .await,
        Err(OrchestratorError::RevisionChanged {
            expected: 7,
            active: 1,
            ..
        })
    ));

    let applied = h
        .orchestrator
        .update("photos", &choices(&["ml:minor", "db:minor"]), 1)
        .await
        .unwrap();
    assert_eq!(applied.revision.revision, 2);
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    assert_eq!(deployment.source, DeploymentSource::Apply);

    // Only the image strings changed: both group members' tags, and db's tag with its digest.
    let expected = photos()
        .replace("immich-server:v2.3.1", "immich-server:v2.4.0")
        .replace(
            "immich-machine-learning:v2.3.1",
            "immich-machine-learning:v2.4.0",
        )
        .replace(
            &format!("postgres:16.4@{}", digest(0x11)),
            &format!("postgres:16.6@{}", digest(0xdd)),
        );
    assert_eq!(applied.revision.toml, expected);

    // What was applied is no longer on offer: the rows are stale until the next check.
    assert!(
        h.orchestrator
            .stack_updates("photos")
            .await
            .unwrap()
            .offers
            .is_empty()
    );
    let err = h
        .orchestrator
        .update("photos", &choices(&["server:minor"]), 2)
        .await
        .unwrap_err();
    assert!(matches!(err, OrchestratorError::NoUpdate(_)), "{err:?}");
    assert!(
        err.to_string().contains("no minor update of server"),
        "{err}"
    );
}

#[tokio::test]
async fn a_moved_tag_rewrites_a_pinned_digest_and_resolves_an_unpinned_tag_again() {
    let h = harness().await;
    let applied = h.apply(&photos()).await;
    h.finish(applied.deployment.as_ref().unwrap()).await;
    h.found("db", &[(UpdateKind::Digest, "16.4")]).await;
    h.found("server", &[(UpdateKind::Digest, "v2.3.1")]).await;

    // db's file pins its digest: the moved tag rewrites it, nothing else in the file changes.
    h.resolver.digest.store(0xcc, Ordering::SeqCst);
    let applied = h
        .orchestrator
        .update("photos", &choices(&["db:digest", "server:digest"]), 1)
        .await
        .unwrap();
    assert_eq!(
        applied.revision.toml,
        photos().replace(&digest(0x11), &digest(0xcc))
    );
    let pins = h.store.revision_images(applied.revision.id).await.unwrap();
    assert_eq!(pins["db"], digest(0xcc));
    // server's tag was resolved again on apply.
    assert_eq!(pins["server"], digest(0xcc));
    assert_eq!(pins["ml"], digest(0xcc));
}
