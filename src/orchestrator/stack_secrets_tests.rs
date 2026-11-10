//! Secrets changed from the UI and "re-resolve tags" (ADR-0025, ADR-0029, ADR-0049).

use std::sync::atomic::Ordering;

use super::tests::{WITH_SECRETS, digest, harness};
use super::*;
use crate::secrets::SecretValue;

use DeploymentState as S;

#[tokio::test]
async fn setting_a_secret_redeploys_only_the_services_that_reference_it() {
    let h = harness().await;
    h.set_secret("notes/database-url", "postgres://one").await;
    h.apply(WITH_SECRETS).await;
    let first = h.deploy(&[("api", 1), ("multi", 1), ("worker", 1)]).await;
    assert_eq!(h.finish(&first).await.state, S::Live);

    let change = h
        .orchestrator
        .set_secret("notes", "database-url", &"postgres://two".into())
        .await
        .unwrap();
    assert_eq!(change.secret, "notes/database-url");
    assert!(change.existed);
    assert_eq!(change.services, ["api"]);
    let deployment = h.finish(change.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    assert_eq!(deployment.source, DeploymentSource::Apply);
    assert_eq!(
        h.generations().await,
        [("api".into(), 2), ("multi".into(), 1), ("worker".into(), 1)]
    );
    let api = h.spec_of("hoster-notes-api-g2");
    assert_eq!(api.secrets["DATABASE_URL"].expose(), "postgres://two");

    // A secret no service references is stored without a deployment.
    let unused = h
        .orchestrator
        .set_secret("notes", "unused", &"x".into())
        .await
        .unwrap();
    assert!(!unused.existed);
    assert!(unused.services.is_empty());
    assert_eq!(unused.deployment, None);

    let listed = h.orchestrator.secrets("notes").await.unwrap();
    let names: Vec<_> = listed.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["database-url", "unused"]);
    assert_eq!(listed[0].services, ["api"]);
    assert!(listed[0].updated_at.is_some());
    assert!(listed[1].services.is_empty());
    // Values are never part of a listing.
    assert!(!format!("{listed:?}").contains("postgres"));
}

#[tokio::test]
async fn deleting_a_referenced_secret_redeploys_and_fails_visibly() {
    let h = harness().await;
    h.set_secret("notes/database-url", "postgres://one").await;
    h.apply(WITH_SECRETS).await;
    let first = h.deploy(&[("api", 1), ("multi", 1), ("worker", 1)]).await;
    assert_eq!(h.finish(&first).await.state, S::Live);

    let change = h
        .orchestrator
        .delete_secret("notes", "database-url")
        .await
        .unwrap();
    assert!(change.existed);
    assert_eq!(change.services, ["api"]);
    let failed = h.finish(change.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Rejected, "{failed:?}");
    assert!(failed.error.unwrap().contains("notes/database-url"));
    // The running container keeps serving, and the stack shows the failure (ADR-0049).
    assert_eq!(h.generations().await[0], ("api".into(), 1));
    let status = h.orchestrator.status().await.unwrap();
    assert!(status[0].flag.as_deref().unwrap().contains("deploy failed"));

    // Still referenced, no longer set.
    let listed = h.orchestrator.secrets("notes").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].updated_at, None);
    assert_eq!(listed[0].services, ["api"]);

    // Deleting what is not set does nothing.
    let again = h
        .orchestrator
        .delete_secret("notes", "database-url")
        .await
        .unwrap();
    assert!(!again.existed);
    assert_eq!(again.deployment, None);
}

/// `api` reads `notes/creds` from a file only, `worker` takes `notes/shared` in its environment
/// and `api` the same secret as a file; `multi` uses no secret.
const WITH_FILE_SECRETS: &str = r#"
version = 1
name = "notes"

[services.api]
image = "ghcr.io/acme/notes-api"

[services.api.files]
"/etc/api/config.yml" = { content = "log: info" }
"/run/secrets/creds.json" = { secret = "notes/creds" }
"/run/secrets/shared" = { secret = "notes/shared" }

[[services.api.routes]]
host = "notes.example.com"
port = 8080

[services.multi]
image = "ghcr.io/acme/notes-multi"
files = { "/etc/multi.conf" = { content = "x" } }

[services.worker]
image = "ghcr.io/acme/notes-worker"
secrets = { SHARED = "notes/shared" }
"#;

/// ADR-0057 §6 and §7: a secret that only a file uses is versioned, listed and redeployed like
/// one in the environment.
#[tokio::test]
async fn a_file_secret_redeploys_only_the_services_that_use_it() {
    let h = harness().await;
    h.set_secret("notes/creds", "one").await;
    h.set_secret("notes/shared", "s1").await;
    h.apply(WITH_FILE_SECRETS).await;
    let first = h.deploy(&[("api", 1), ("multi", 1), ("worker", 1)]).await;
    assert_eq!(h.finish(&first).await.state, S::Live);

    let change = h
        .orchestrator
        .set_secret("notes", "creds", &"two".into())
        .await
        .unwrap();
    assert!(change.existed);
    assert_eq!(change.services, ["api"]);
    let deployment = h.finish(change.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    assert_eq!(
        h.generations().await,
        [("api".into(), 2), ("multi".into(), 1), ("worker".into(), 1)]
    );

    // The same digests again change nothing: the version is part of the hash, not the time.
    let again = h.deploy(&[("api", 1), ("multi", 1), ("worker", 1)]).await;
    assert_eq!(h.finish(&again).await.state, S::Noop);
    assert_eq!(h.generations().await[0], ("api".into(), 2));

    // A secret used as a file by one service and in the environment of another moves both.
    let change = h
        .orchestrator
        .set_secret("notes", "shared", &"s2".into())
        .await
        .unwrap();
    assert_eq!(change.services, ["api", "worker"]);
    let deployment = h.finish(change.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    assert_eq!(
        h.generations().await,
        [("api".into(), 3), ("multi".into(), 1), ("worker".into(), 3)]
    );

    let listed = h.orchestrator.secrets("notes").await.unwrap();
    let users: Vec<_> = listed
        .iter()
        .map(|s| (s.name.as_str(), s.services.join(",")))
        .collect();
    assert_eq!(
        users,
        [
            ("creds", "api".to_owned()),
            ("shared", "api,worker".to_owned())
        ]
    );
}

#[tokio::test]
async fn a_missing_file_secret_rejects_the_deployment_by_name_and_path() {
    let h = harness().await;
    h.set_secret("notes/creds", "s3cret-value").await;
    h.set_secret("notes/shared", "s1").await;
    h.apply(WITH_FILE_SECRETS).await;
    let first = h.deploy(&[("api", 1), ("multi", 1), ("worker", 1)]).await;
    assert_eq!(h.finish(&first).await.state, S::Live);

    let change = h
        .orchestrator
        .delete_secret("notes", "creds")
        .await
        .unwrap();
    assert_eq!(change.services, ["api"]);
    let failed = h.finish(change.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Rejected, "{failed:?}");
    assert_eq!(
        failed.error.as_deref(),
        Some("service api: secret `notes/creds` (for file /run/secrets/creds.json) is not set")
    );
    assert_eq!(h.generations().await[0], ("api".into(), 1));

    // Still referenced, no longer set.
    let listed = h.orchestrator.secrets("notes").await.unwrap();
    assert_eq!(listed[0].name, "creds");
    assert_eq!(listed[0].updated_at, None);
    assert_eq!(listed[0].services, ["api"]);
}

#[tokio::test]
async fn secret_names_are_checked_and_the_stack_must_exist() {
    let h = harness().await;
    let value = SecretValue::from("v");
    assert!(matches!(
        h.orchestrator.set_secret("ghost", "db", &value).await,
        Err(OrchestratorError::UnknownStack(_))
    ));
    h.apply(WITH_SECRETS).await;
    for bad in ["a/b", "..", "", "sp ace"] {
        let err = h
            .orchestrator
            .set_secret("notes", bad, &value)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                OrchestratorError::Secret(SecretError::InvalidName { .. })
            ),
            "{bad:?}: {err:?}"
        );
    }
    assert!(matches!(
        h.orchestrator.secrets("ghost").await,
        Err(OrchestratorError::UnknownStack(_))
    ));
}

#[tokio::test]
async fn reresolve_applies_and_redeploys_only_when_a_digest_moved() {
    let h = harness().await;
    h.bootstrap().await;

    // Unchanged upstream: the preview shows no move, and confirming creates nothing.
    let preview = h.orchestrator.reresolve_preview("notes").await.unwrap();
    assert_eq!(preview.pins.len(), 1);
    assert_eq!(preview.pins[0].before, Some(digest(0xaa)));
    assert_eq!(preview.pins[0].after, digest(0xaa));
    assert!(preview.changed.is_empty());
    assert!(
        h.orchestrator
            .reresolve("notes", 1)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(h.store.revisions("notes").await.unwrap().len(), 1);

    // The tag moves upstream.
    h.resolver.digest.store(0xbb, Ordering::SeqCst);
    let preview = h.orchestrator.reresolve_preview("notes").await.unwrap();
    assert_eq!(preview.pins[0].after, digest(0xbb));
    assert_eq!(preview.changed, ["storage"]);
    // A stale confirmation is refused.
    assert!(matches!(
        h.orchestrator.reresolve("notes", 7).await,
        Err(OrchestratorError::RevisionChanged {
            expected: 7,
            active: 1,
            ..
        })
    ));
    let applied = h.orchestrator.reresolve("notes", 1).await.unwrap().unwrap();
    assert_eq!(applied.revision.revision, 2);
    assert_eq!(applied.changed, ["storage"]);
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    assert_eq!(
        h.store.revision_images(applied.revision.id).await.unwrap()["storage"],
        digest(0xbb)
    );
    let storage = h
        .store
        .services("notes")
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.service == "storage")
        .unwrap();
    assert_eq!(storage.digest, digest(0xbb));

    assert!(matches!(
        h.orchestrator.reresolve_preview("ghost").await,
        Err(OrchestratorError::UnknownStack(_))
    ));
}
