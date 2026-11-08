//! Scenario tests: the orchestrator against [`FakeRuntime`] and a real (temporary) store.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;

use super::*;
use crate::config::PortRange;
use crate::runtime::{ContainerInfo, ContainerState, FakeRuntime, Operation};
use crate::secrets::{SecretKey, SecretName};
use crate::stack::ImageRef;

pub(super) const NOTES: &str = r#"
version = 1
name = "notes"

[services.api]
image = "ghcr.io/acme/notes-api"

[[services.api.routes]]
host = "notes.example.com"
prefix = "/api"
port = 8080

[services.web]
image = "ghcr.io/acme/notes-web"

[[services.web.routes]]
host = "notes.example.com"
port = 80

[services.storage]
image = "docker.io/rustfs/rustfs:1.0.0"
update = "stop-first"

[services.storage.volumes]
data = "/data"

[[services.storage.routes]]
host = "s3.notes.example.com"
port = 9000
"#;

pub(super) fn digest(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

/// Resolves every tag to `digest(0xaa)` and counts the lookups.
#[derive(Default)]
pub(super) struct FakeResolver {
    calls: AtomicUsize,
}

#[async_trait]
impl TagResolver for FakeResolver {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(image.clone().pin(digest(0xaa).parse().unwrap()))
    }
}

pub(super) struct Harness {
    pub(super) _dir: TempDir,
    pub(super) store: Store,
    pub(super) runtime: Arc<FakeRuntime>,
    pub(super) resolver: Arc<FakeResolver>,
    pub(super) secrets: SecretStore,
    pub(super) orchestrator: Arc<Orchestrator<FakeRuntime>>,
}

pub(super) async fn harness_with(settle: Duration) -> Harness {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let runtime = Arc::new(FakeRuntime::new());
    let resolver = Arc::new(FakeResolver::default());
    let ports = PortAllocator::new(store.clone(), PortRange::try_from(20000..=20015).unwrap());
    let settings = Settings {
        settle,
        settle_poll: Duration::from_millis(5),
        ..Settings::default()
    };
    let secrets = SecretStore::new(store.clone(), &SecretKey::generate());
    let orchestrator = Orchestrator::new(
        store.clone(),
        Arc::clone(&runtime),
        Arc::clone(&resolver) as Arc<dyn TagResolver>,
        ports,
        secrets.clone(),
        settings,
    );
    Harness {
        _dir: dir,
        store,
        runtime,
        resolver,
        secrets,
        orchestrator,
    }
}

pub(super) async fn harness() -> Harness {
    harness_with(Duration::from_millis(20)).await
}

impl Harness {
    pub(super) async fn apply(&self, toml: &str) -> ApplyOutcome {
        self.orchestrator.apply(toml).await.unwrap()
    }

    pub(super) async fn deploy(&self, images: &[(&str, u8)]) -> DeployOutcome {
        let images = images
            .iter()
            .map(|(s, d)| ((*s).to_owned(), digest(*d)))
            .collect();
        self.orchestrator
            .deploy("notes", images, DeploymentSource::Webhook)
            .await
            .unwrap()
    }

    pub(super) async fn finish(&self, outcome: &DeployOutcome) -> Deployment {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.orchestrator.wait(outcome.deployment),
        )
        .await
        .expect("deployment finishes")
        .unwrap()
    }

    /// Applies NOTES (storage deploys) and deploys api and web: every service at a digest.
    pub(super) async fn bootstrap(&self) {
        let applied = self.apply(NOTES).await;
        let first = self.finish(applied.deployment.as_ref().unwrap()).await;
        assert_eq!(first.state, DeploymentState::Live, "{first:?}");
        let deployed = self.deploy(&[("api", 1), ("web", 1)]).await;
        let second = self.finish(&deployed).await;
        assert_eq!(second.state, DeploymentState::Live, "{second:?}");
    }

    pub(super) fn container_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .runtime
            .containers()
            .into_iter()
            .map(|c| c.name)
            .collect();
        names.sort();
        names
    }

    pub(super) fn container(&self, name: &str) -> ContainerInfo {
        self.runtime
            .containers()
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no container {name}"))
    }

    pub(super) async fn generations(&self) -> Vec<(String, u32)> {
        self.store
            .services("notes")
            .await
            .unwrap()
            .into_iter()
            .map(|p| (p.service, p.generation))
            .collect()
    }

    pub(super) async fn states(&self, id: DeploymentId) -> Vec<DeploymentState> {
        self.store
            .events(id)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.to)
            .collect()
    }
}

use DeploymentState as S;

#[tokio::test]
async fn happy_path_deploys_every_service_and_records_each_transition() {
    let h = harness().await;
    let applied = h.apply(NOTES).await;
    assert_eq!(applied.revision.revision, 1);
    // Third-party tags are resolved at apply and stored with the revision (ADR-0025).
    assert_eq!(h.resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.store.revision_images(applied.revision.id).await.unwrap(),
        DigestSet::from([("storage".into(), digest(0xaa))])
    );
    // Only storage has a digest; api and web await theirs.
    assert_eq!(applied.changed, ["storage"]);
    let first = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(first.state, S::Live);
    assert_eq!(first.source, DeploymentSource::Apply);
    let status = h.orchestrator.status().await.unwrap();
    let services: Vec<_> = status[0]
        .services
        .iter()
        .map(|s| (s.name.as_str(), s.state.clone()))
        .collect();
    assert_eq!(
        services,
        [
            ("api", ServiceState::AwaitingFirstDigest),
            (
                "storage",
                ServiceState::Running {
                    generation: 1,
                    digest: digest(0xaa)
                }
            ),
            ("web", ServiceState::AwaitingFirstDigest),
        ]
    );

    let deployed = h.deploy(&[("api", 1), ("web", 1)]).await;
    let second = h.finish(&deployed).await;
    assert_eq!(second.state, S::Live);
    assert_eq!(second.generation, Some(2));
    assert!(second.committed_at.is_some());
    assert_eq!(
        h.states(second.id).await,
        [
            S::Received,
            S::Validating,
            S::Pulling,
            S::Starting,
            S::HealthChecking,
            S::Replacing,
            S::Switching,
            S::Draining,
            S::CleaningUp,
            S::Live,
        ]
    );
    let events = h.store.events(second.id).await.unwrap();
    assert!(events.windows(2).all(|w| w[0].at <= w[1].at));
    // storage was not recreated and keeps its generation (stack-format §3.1).
    assert_eq!(
        h.generations().await,
        [("api".into(), 2), ("storage".into(), 1), ("web".into(), 2)]
    );
    assert_eq!(
        h.container_names(),
        [
            "hoster-notes-api-g2",
            "hoster-notes-storage-g1",
            "hoster-notes-web-g2"
        ]
    );
    let api = h.container("hoster-notes-api-g2");
    assert_eq!(api.state, ContainerState::Running);
    assert_eq!(api.labels[label::DIGEST], digest(1));
    assert_eq!(api.labels[label::GENERATION], "2");
    assert_eq!(api.labels[label::DEPLOYMENT], second.id.to_string());
    assert!(
        api.ports
            .iter()
            .all(|p| p.is_loopback() && p.container_port == 8080)
    );
    let spec = h.runtime.spec(&api.id).unwrap();
    assert_eq!(spec.env["PORT"], "8080");
    assert_eq!(spec.network(), "hoster-notes");
    let storage = h
        .runtime
        .spec(&h.container("hoster-notes-storage-g1").id)
        .unwrap();
    assert_eq!(storage.volumes[0].volume(), "hoster-notes-data");
    assert_eq!(h.runtime.networks()[0].name, "hoster-notes");
    // The pointers own the ports of the running containers, and nothing else is reserved.
    let pointer_ports: Vec<u16> = h
        .store
        .services("notes")
        .await
        .unwrap()
        .iter()
        .flat_map(|p| p.ports.values().copied())
        .collect();
    assert_eq!(pointer_ports.len(), 3);
    assert_eq!(h.store.port_allocations().await.unwrap().len(), 3);
}

#[tokio::test]
async fn deploying_what_runs_is_a_noop() {
    let h = harness().await;
    h.bootstrap().await;
    let before = h.container_names();
    let outcome = h.deploy(&[("api", 1)]).await;
    assert_eq!(outcome.state, S::Noop);
    let noop = h
        .store
        .deployment(outcome.deployment)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(noop.state, S::Noop);
    assert_eq!(h.states(noop.id).await, [S::Received, S::Noop]);
    assert_eq!(h.container_names(), before);
    // Re-applying the same file changes nothing and starts no deployment.
    let again = h.apply(NOTES).await;
    assert!(again.deployment.is_none());
    assert!(again.changed.is_empty());
}

#[tokio::test]
async fn apply_with_a_config_change_redeploys_only_that_service() {
    let h = harness().await;
    h.bootstrap().await;
    let edited = NOTES.replace(
        "prefix = \"/api\"",
        "prefix = \"/api\"\nstrip_prefix = true",
    );
    let applied = h.apply(&edited).await;
    assert_eq!(applied.changed, ["api"]);
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live);
    assert_eq!(deployment.source, DeploymentSource::Apply);
    assert_eq!(
        h.generations().await,
        [("api".into(), 3), ("storage".into(), 1), ("web".into(), 2)]
    );
    assert_eq!(
        h.container_names(),
        [
            "hoster-notes-api-g3",
            "hoster-notes-storage-g1",
            "hoster-notes-web-g2"
        ]
    );
    // The old api port was released with its container.
    assert_eq!(h.store.port_allocations().await.unwrap().len(), 3);
}

#[tokio::test]
async fn preview_shows_what_an_apply_would_do_without_doing_it() {
    let h = harness().await;
    let first = h.orchestrator.preview(NOTES).await.unwrap();
    assert_eq!(first.stack, "notes");
    assert!(first.current.is_none(), "a new stack");
    assert_eq!(
        first.pins,
        [PinChange {
            service: "storage".into(),
            before: None,
            after: digest(0xaa),
        }]
    );
    assert_eq!(first.changed, ["storage"]);
    assert!(
        h.store.stack("notes").await.unwrap().is_none(),
        "nothing stored"
    );
    assert!(h.runtime.containers().is_empty(), "nothing deployed");

    h.bootstrap().await;
    let edited = NOTES.replace(
        "prefix = \"/api\"",
        "prefix = \"/api\"\nstrip_prefix = true",
    );
    let preview = h.orchestrator.preview(&edited).await.unwrap();
    let current = preview.current.expect("the active revision");
    assert_eq!(current.revision, 1);
    assert_eq!(current.toml, NOTES);
    assert_eq!(preview.pins[0].before, Some(digest(0xaa)));
    assert_eq!(preview.changed, ["api"]);
    assert_eq!(h.store.revisions("notes").await.unwrap().len(), 1);

    let unchanged = h.orchestrator.preview(NOTES).await.unwrap();
    assert!(unchanged.changed.is_empty());

    let err = h
        .orchestrator
        .preview("version = 1\nname = \"Bad\"\n")
        .await
        .unwrap_err();
    assert!(matches!(err, OrchestratorError::Invalid(_)), "{err}");
}

#[tokio::test]
async fn deployment_lookup_returns_the_transitions_of_that_stack_only() {
    let h = harness().await;
    let applied = h.apply(NOTES).await;
    let outcome = applied.deployment.unwrap();
    h.finish(&outcome).await;
    let (deployment, events) = h
        .orchestrator
        .deployment("notes", outcome.deployment)
        .await
        .unwrap();
    assert_eq!(deployment.state, S::Live);
    assert_eq!(events.last().unwrap().to, S::Live);
    assert!(matches!(
        h.orchestrator.deployment("other", outcome.deployment).await,
        Err(OrchestratorError::UnknownDeployment(_))
    ));
}

#[tokio::test]
async fn pull_failure_aborts_and_leaves_the_old_generation() {
    let h = harness().await;
    h.bootstrap().await;
    let before = h.container_names();
    h.runtime.fail_next(
        Operation::PullImage,
        RuntimeError::NotFound {
            kind: ObjectKind::Image,
            name: "ghcr.io/acme/notes-api".into(),
        },
    );
    let outcome = h.deploy(&[("api", 2)]).await;
    let failed = h.finish(&outcome).await;
    assert_eq!(failed.state, S::Failed);
    assert!(failed.error.as_deref().unwrap().contains("cannot pull"));
    assert_eq!(
        h.states(failed.id).await,
        [
            S::Received,
            S::Validating,
            S::Pulling,
            S::Aborting,
            S::Failed
        ]
    );
    assert_eq!(h.container_names(), before);
    assert_eq!(h.generations().await[0], ("api".into(), 2));
    // The ports reserved for g3 are released again.
    assert_eq!(h.store.port_allocations().await.unwrap().len(), 3);
}

#[tokio::test]
async fn start_failure_removes_the_new_container() {
    let h = harness().await;
    h.bootstrap().await;
    let before = h.container_names();
    h.runtime.fail_next(
        Operation::Start,
        RuntimeError::Api {
            operation: Operation::Start,
            message: "boom".into(),
        },
    );
    let failed = h.finish(&h.deploy(&[("api", 2)]).await).await;
    assert_eq!(failed.state, S::Failed);
    assert!(failed.error.as_deref().unwrap().contains("cannot start"));
    assert_eq!(h.container_names(), before);
    assert_eq!(h.store.port_allocations().await.unwrap().len(), 3);
}

#[tokio::test]
async fn a_container_exiting_during_the_health_check_aborts() {
    let h = harness_with(Duration::from_millis(500)).await;
    h.bootstrap().await;
    let before = h.container_names();
    let outcome = h.deploy(&[("api", 2)]).await;
    // Crash the new container while the built-in check watches it.
    let runtime = Arc::clone(&h.runtime);
    tokio::spawn(async move {
        loop {
            if let Some(c) = runtime
                .containers()
                .into_iter()
                .find(|c| c.name == "hoster-notes-api-g3" && c.state.is_running())
            {
                runtime.set_state(&c.id, ContainerState::Exited(Some(1)));
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
    let failed = h.finish(&outcome).await;
    assert_eq!(failed.state, S::Failed);
    assert!(
        failed.error.as_deref().unwrap().contains("exited"),
        "{failed:?}"
    );
    assert!(h.states(failed.id).await.contains(&S::HealthChecking));
    assert_eq!(h.container_names(), before);
}

#[tokio::test]
async fn stop_first_failure_restarts_the_old_container() {
    let h = harness().await;
    h.bootstrap().await;
    let edited = NOTES.replace(
        "data = \"/data\"",
        "data = \"/data\"\n\n[services.storage.env]\nA = \"1\"",
    );
    // In a storage-only deployment the first start is the new storage container's.
    h.runtime.fail_next(
        Operation::Start,
        RuntimeError::Api {
            operation: Operation::Start,
            message: "boom".into(),
        },
    );
    let applied = h.apply(&edited).await;
    assert_eq!(applied.changed, ["storage"]);
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed);
    assert_eq!(
        h.states(failed.id).await,
        [
            S::Received,
            S::Validating,
            S::Pulling,
            S::Starting,
            S::HealthChecking,
            S::Replacing,
            S::Aborting,
            S::Failed
        ]
    );
    let old = h.container("hoster-notes-storage-g1");
    assert_eq!(old.state, ContainerState::Running, "old storage restarted");
    assert!(
        !h.container_names()
            .contains(&"hoster-notes-storage-g3".to_owned())
    );
    // The revision stays active and the stack is flagged (ADR-0049).
    let status = h.orchestrator.status().await.unwrap();
    assert_eq!(status[0].revision.revision, 2);
    let flag = status[0].flag.as_deref().unwrap();
    assert!(flag.starts_with("revision 2: deploy failed"), "{flag}");
}

#[tokio::test]
async fn stop_first_replacement_stops_old_before_starting_new() {
    let h = harness().await;
    h.bootstrap().await;
    let edited = NOTES.replace(
        "data = \"/data\"",
        "data = \"/data\"\n\n[services.storage.env]\nA = \"1\"",
    );
    let applied = h.apply(&edited).await;
    let live = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(live.state, S::Live);
    let calls = h.runtime.calls();
    let stop = calls.iter().rposition(|c| *c == Operation::Stop).unwrap();
    let create = calls
        .iter()
        .rposition(|c| *c == Operation::CreateContainer)
        .unwrap();
    assert!(stop < create, "{calls:?}");
    assert_eq!(
        h.container_names(),
        [
            "hoster-notes-api-g2",
            "hoster-notes-storage-g3",
            "hoster-notes-web-g2"
        ]
    );
}

#[tokio::test]
async fn a_stack_has_one_active_deployment_and_queues_the_newest() {
    let h = harness_with(Duration::from_millis(300)).await;
    h.bootstrap().await;
    let first = h.deploy(&[("api", 2)]).await;
    assert_ne!(first.state, S::Queued);
    let second = h.deploy(&[("api", 3)]).await;
    let third = h.deploy(&[("api", 4)]).await;
    assert_eq!(second.state, S::Queued);
    assert_eq!(third.state, S::Queued);
    assert_eq!(h.finish(&second).await.state, S::Superseded);
    assert_eq!(h.finish(&first).await.state, S::Live);
    let last = h.finish(&third).await;
    assert_eq!(last.state, S::Live);
    assert_eq!(
        h.states(last.id).await[..3],
        [S::Received, S::Queued, S::Received]
    );
    let pointers = h.store.services("notes").await.unwrap();
    assert_eq!(pointers[0].digest, digest(4));
}

#[tokio::test]
async fn a_queued_deployment_records_the_target_it_actually_deploys() {
    let h = harness_with(Duration::from_millis(300)).await;
    h.bootstrap().await;
    let first = h.deploy(&[("web", 2)]).await;
    let queued = h.deploy(&[("api", 3)]).await;
    assert_eq!(queued.state, S::Queued);
    assert_eq!(h.finish(&first).await.state, S::Live);
    let done = h.finish(&queued).await;
    assert_eq!(done.state, S::Live);
    // web was at digest 1 when the request was queued, and at 2 when it started: a rollback
    // to this deployment must not move web back.
    assert_eq!(
        done.target,
        DigestSet::from([
            ("api".into(), digest(3)),
            ("storage".into(), digest(0xaa)),
            ("web".into(), digest(2)),
        ])
    );
}

#[tokio::test]
async fn rollback_to_an_older_revision_checks_its_routes_again() {
    let h = harness().await;
    h.bootstrap().await;
    let good = h.store.deployments("notes", 1).await.unwrap().remove(0);
    // notes moves its storage to another host, and a second stack takes the old one.
    let moved = NOTES.replace("s3.notes.example.com", "s3.example.org");
    h.finish(h.apply(&moved).await.deployment.as_ref().unwrap())
        .await;
    let other = r#"
version = 1
name = "other"

[services.s3]
image = "docker.io/rustfs/rustfs:1.0.0"

[[services.s3.routes]]
host = "s3.notes.example.com"
port = 9000
"#;
    h.finish(h.apply(other).await.deployment.as_ref().unwrap())
        .await;
    let err = h.orchestrator.rollback("notes", good.id).await.unwrap_err();
    assert!(matches!(err, OrchestratorError::Collision(_)), "{err}");
    // The refused rollback did not become the active revision.
    let active = h.store.latest_revision("notes").await.unwrap().unwrap();
    assert_eq!(active.revision, 2);
}

#[tokio::test]
async fn rollback_redeploys_an_earlier_deployment() {
    let h = harness().await;
    h.bootstrap().await;
    let good = h.store.deployments("notes", 1).await.unwrap().remove(0);
    h.finish(&h.deploy(&[("api", 2)]).await).await;
    let outcome = h.orchestrator.rollback("notes", good.id).await.unwrap();
    let rolled = h.finish(&outcome).await;
    assert_eq!(rolled.state, S::Live);
    assert_eq!(rolled.source, DeploymentSource::Rollback);
    assert_eq!(
        h.store.services("notes").await.unwrap()[0].digest,
        digest(1)
    );
}

#[tokio::test]
async fn typed_errors_for_bad_requests() {
    let h = harness().await;
    let err = h
        .orchestrator
        .deploy("nope", DigestSet::new(), DeploymentSource::Manual)
        .await
        .unwrap_err();
    assert!(matches!(err, OrchestratorError::UnknownStack(_)), "{err}");
    h.apply(NOTES).await;
    let deploy = |service: &str, d: String| {
        h.orchestrator.deploy(
            "notes",
            DigestSet::from([(service.to_owned(), d)]),
            DeploymentSource::Manual,
        )
    };
    assert!(matches!(
        deploy("storage", digest(1)).await.unwrap_err(),
        OrchestratorError::NotCiBuilt(_)
    ));
    assert!(matches!(
        deploy("db", digest(1)).await.unwrap_err(),
        OrchestratorError::UnknownService(_)
    ));
    assert!(matches!(
        deploy("api", "latest".into()).await.unwrap_err(),
        OrchestratorError::Digest { .. }
    ));
    assert!(matches!(
        h.orchestrator.apply("version = 1").await.unwrap_err(),
        OrchestratorError::Invalid(_)
    ));
    // Another stack may not claim notes' routes (ADR-0012).
    let thief = NOTES.replace("name = \"notes\"", "name = \"thief\"");
    assert!(matches!(
        h.orchestrator.apply(&thief).await.unwrap_err(),
        OrchestratorError::Collision(_)
    ));
}

#[tokio::test]
async fn remove_deletes_containers_network_and_pointers_but_keeps_history_and_volumes() {
    let h = harness().await;
    h.bootstrap().await;
    let history = h.store.deployments("notes", 100).await.unwrap();
    let removed = h
        .orchestrator
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    assert_eq!(removed.containers, 3);
    assert!(removed.volumes.is_empty());
    assert!(h.runtime.containers().is_empty());
    assert!(h.runtime.networks().is_empty());
    assert_eq!(h.runtime.volume_names(), ["hoster-notes-data"]);
    assert!(!h.runtime.calls().contains(&Operation::RemoveVolume));
    assert!(h.store.stack("notes").await.unwrap().is_none());
    assert!(h.store.services("notes").await.unwrap().is_empty());
    assert!(h.store.port_allocations().await.unwrap().is_empty());
    assert!(h.orchestrator.status().await.unwrap().is_empty());
    // The history is retained.
    assert_eq!(h.store.deployments("notes", 100).await.unwrap(), history);
    assert_eq!(h.store.revisions("notes").await.unwrap().len(), 1);

    assert!(matches!(
        h.orchestrator
            .remove(RemoveStack::keeping_volumes("notes"))
            .await
            .unwrap_err(),
        OrchestratorError::UnknownStack(_)
    ));
    // Neither the webhook nor a rollback revives a removed stack.
    assert!(matches!(
        h.orchestrator
            .deploy(
                "notes",
                DigestSet::from([("api".into(), digest(5))]),
                DeploymentSource::Webhook
            )
            .await
            .unwrap_err(),
        OrchestratorError::UnknownStack(_)
    ));
    assert!(matches!(
        h.orchestrator
            .rollback("notes", history[0].id)
            .await
            .unwrap_err(),
        OrchestratorError::UnknownStack(_)
    ));
}

#[tokio::test]
async fn a_removed_stack_can_be_applied_again_and_reuses_its_volume() {
    let h = harness().await;
    h.bootstrap().await;
    let before = h.generations().await;
    h.orchestrator
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    let applied = h.apply(NOTES).await;
    let done = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(done.state, S::Live, "{done:?}");
    assert_eq!(applied.revision.revision, 2);
    // Generations carry on, so container names never repeat.
    let storage = |g: &[(String, u32)]| g.iter().find(|(s, _)| s == "storage").unwrap().1;
    assert!(storage(&h.generations().await) > storage(&before));
    assert_eq!(h.runtime.volume_names(), ["hoster-notes-data"]);
}

#[tokio::test]
async fn deleting_volumes_needs_the_stack_name_as_confirmation() {
    let h = harness().await;
    h.bootstrap().await;
    let unconfirmed = RemoveStack {
        confirmation: None,
        ..RemoveStack::deleting_volumes("notes", "notes")
    };
    for request in [unconfirmed, RemoveStack::deleting_volumes("notes", "Notes")] {
        assert!(matches!(
            h.orchestrator.remove(request).await.unwrap_err(),
            OrchestratorError::VolumeConfirmation { .. }
        ));
    }
    // Refused before anything was touched.
    assert_eq!(h.runtime.containers().len(), 3);
    assert!(h.store.stack("notes").await.unwrap().is_some());

    let removed = h
        .orchestrator
        .remove(RemoveStack::deleting_volumes("notes", "notes"))
        .await
        .unwrap();
    assert_eq!(removed.containers, 3);
    assert_eq!(removed.volumes, ["hoster-notes-data"]);
    assert!(h.runtime.volume_names().is_empty());
    assert!(h.store.stack("notes").await.unwrap().is_none());
}

#[tokio::test]
async fn the_kept_volumes_of_a_removed_stack_can_be_deleted_later() {
    let h = harness().await;
    h.bootstrap().await;
    h.orchestrator
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    assert_eq!(h.runtime.volume_names(), ["hoster-notes-data"]);

    // Still only with the confirmation.
    assert!(matches!(
        h.orchestrator
            .remove(RemoveStack::deleting_volumes("notes", "nope"))
            .await
            .unwrap_err(),
        OrchestratorError::VolumeConfirmation { .. }
    ));
    let deleted = h
        .orchestrator
        .remove(RemoveStack::deleting_volumes("notes", "notes"))
        .await
        .unwrap();
    assert_eq!(deleted.containers, 0);
    assert_eq!(deleted.volumes, ["hoster-notes-data"]);
    assert!(h.runtime.volume_names().is_empty());
    // The stack stays removed, with its history.
    assert!(h.store.stack("notes").await.unwrap().is_none());
    assert_eq!(h.store.revisions("notes").await.unwrap().len(), 1);

    // A stack that never existed has no volumes to delete.
    assert!(matches!(
        h.orchestrator
            .remove(RemoveStack::deleting_volumes("ghost", "ghost"))
            .await
            .unwrap_err(),
        OrchestratorError::UnknownStack(_)
    ));
}

#[tokio::test]
async fn removing_a_stack_mid_deploy_aborts_the_deploy_first() {
    // The health check would take a minute; the removal must not wait for it.
    let h = harness_with(Duration::from_secs(60)).await;
    let applied = h.apply(NOTES).await;
    let active = applied.deployment.unwrap();
    assert_ne!(active.state, S::Queued);
    let queued = h.deploy(&[("api", 1)]).await;
    assert_eq!(queued.state, S::Queued);

    let removed = tokio::time::timeout(
        Duration::from_secs(10),
        h.orchestrator.remove(RemoveStack::keeping_volumes("notes")),
    )
    .await
    .expect("removal does not wait for the health check")
    .unwrap();
    assert_eq!(removed.containers, 0, "the abort removed the new container");

    let aborted = h
        .store
        .deployment(active.deployment)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(aborted.state, S::Failed | S::Rejected),
        "{aborted:?}"
    );
    assert_eq!(aborted.error.as_deref(), Some("stack removed"));
    let superseded = h
        .store
        .deployment(queued.deployment)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(superseded.state, S::Superseded);
    assert!(h.runtime.containers().is_empty());
    assert!(h.runtime.networks().is_empty());
    assert!(h.store.port_allocations().await.unwrap().is_empty());
    assert!(h.store.stack("notes").await.unwrap().is_none());
    assert!(h.orchestrator.status().await.unwrap().is_empty());
}

/// A stack whose api reads a secret, plus a two-port service and an unrouted worker.
const WITH_SECRETS: &str = r#"
version = 1
name = "notes"

[services.api]
image = "ghcr.io/acme/notes-api"
env = { LOG = "info" }
secrets = { DATABASE_URL = "notes/database-url" }

[[services.api.routes]]
host = "notes.example.com"
port = 8080

[services.multi]
image = "ghcr.io/acme/notes-multi"

[[services.multi.routes]]
host = "multi.notes.example.com"
port = 8080

[[services.multi.routes]]
host = "multi.notes.example.com"
prefix = "/admin"
port = 9090

[services.worker]
image = "ghcr.io/acme/notes-worker"
"#;

impl Harness {
    async fn set_secret(&self, name: &str, value: &str) {
        self.secrets
            .set(&name.parse::<SecretName>().unwrap(), &value.into())
            .await
            .unwrap();
    }

    fn spec_of(&self, name: &str) -> crate::runtime::ContainerSpec {
        self.runtime.spec(&self.container(name).id).unwrap()
    }
}

#[tokio::test]
async fn secrets_are_injected_at_create_and_port_only_for_one_container_port() {
    let h = harness().await;
    h.set_secret("notes/database-url", "postgres://s3cret")
        .await;
    h.apply(WITH_SECRETS).await;
    let deployed = h.deploy(&[("api", 1), ("multi", 1), ("worker", 1)]).await;
    assert_eq!(h.finish(&deployed).await.state, S::Live);

    let api = h.spec_of("hoster-notes-api-g1");
    assert_eq!(api.secrets["DATABASE_URL"].expose(), "postgres://s3cret");
    assert!(!api.env.contains_key("DATABASE_URL"));
    assert_eq!(api.env["LOG"], "info");
    assert_eq!(api.env["PORT"], "8080");
    // Nothing that prints a spec shows the value.
    assert!(!format!("{api:?}").contains("s3cret"));
    let events = h.store.events(deployed.deployment).await.unwrap();
    assert!(
        events
            .iter()
            .all(|e| !e.message.as_deref().unwrap_or("").contains("s3cret"))
    );

    // Two distinct container ports, or none: no `PORT`.
    assert!(!h.spec_of("hoster-notes-multi-g1").env.contains_key("PORT"));
    assert!(h.spec_of("hoster-notes-worker-g1").env.is_empty());
}

#[tokio::test]
async fn a_missing_secret_rejects_the_deployment_naming_the_secret() {
    let h = harness().await;
    h.apply(WITH_SECRETS).await;
    let deployed = h.deploy(&[("api", 1)]).await;
    let rejected = h.finish(&deployed).await;
    assert_eq!(rejected.state, S::Rejected, "{rejected:?}");
    let error = rejected.error.unwrap();
    assert!(error.contains("notes/database-url"), "{error}");
    assert!(error.contains("api"), "{error}");
    assert!(h.runtime.containers().is_empty());
    assert!(h.store.port_allocations().await.unwrap().is_empty());
}

#[tokio::test]
async fn replacing_a_secret_changes_the_config_hash_of_its_services_only() {
    let h = harness().await;
    h.set_secret("notes/database-url", "postgres://one").await;
    h.apply(WITH_SECRETS).await;
    let first = h.deploy(&[("api", 1), ("multi", 1), ("worker", 1)]).await;
    assert_eq!(h.finish(&first).await.state, S::Live);

    // Same digests, same secret: nothing to do.
    assert_eq!(h.deploy(&[("api", 1)]).await.state, S::Noop);

    h.set_secret("notes/database-url", "postgres://two").await;
    let second = h.deploy(&[]).await;
    assert_eq!(h.finish(&second).await.state, S::Live);
    let generations = h.generations().await;
    assert_eq!(
        generations,
        [("api".into(), 2), ("multi".into(), 1), ("worker".into(), 1)]
    );
    let api = h.spec_of("hoster-notes-api-g2");
    assert_eq!(api.secrets["DATABASE_URL"].expose(), "postgres://two");
}

#[tokio::test]
async fn volumes_are_created_labelled_before_their_container_and_kept_on_removal() {
    let h = harness().await;
    h.bootstrap().await;
    let volumes = h.runtime.volumes();
    assert_eq!(volumes.len(), 1);
    assert_eq!(volumes[0].name, "hoster-notes-data");
    assert_eq!(volumes[0].labels[label::MANAGED], "true");
    assert_eq!(volumes[0].labels[label::STACK], "notes");
    let calls = h.runtime.calls();
    let ensure = calls
        .iter()
        .position(|c| *c == Operation::EnsureVolume)
        .unwrap();
    assert!(calls[ensure + 1..].contains(&Operation::CreateContainer));

    h.orchestrator
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    assert!(h.runtime.containers().is_empty());
    assert_eq!(h.runtime.volumes(), volumes);
}

#[tokio::test]
async fn a_volume_that_cannot_be_created_fails_the_deploy() {
    let h = harness().await;
    h.runtime.fail_always(
        Operation::EnsureVolume,
        RuntimeError::Api {
            operation: Operation::EnsureVolume,
            message: "disk full".into(),
        },
    );
    let applied = h.apply(NOTES).await;
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    assert!(failed.error.unwrap().contains("hoster-notes-data"));
    assert!(h.runtime.containers().is_empty());
}

/// `hoster-<stack>-<volume>` is ambiguous when names contain `-`: stack `a-b` volume `c` and stack
/// `a` volume `b-c` are both `hoster-a-b-c`. The second stack must not mount the first's data.
#[tokio::test]
async fn a_volume_labelled_for_another_stack_is_never_mounted() {
    let h = harness().await;
    let stack = |name: &str, volume: &str| {
        format!(
            "version = 1\nname = \"{name}\"\n\n[services.db]\n\
             image = \"docker.io/library/postgres:17\"\nupdate = \"stop-first\"\n\n\
             [services.db.volumes]\n{volume} = \"/data\"\n"
        )
    };
    let first = h.apply(&stack("a-b", "c")).await;
    let first = h.finish(first.deployment.as_ref().unwrap()).await;
    assert_eq!(first.state, S::Live, "{first:?}");

    let second = h.apply(&stack("a", "b-c")).await;
    let second = h.finish(second.deployment.as_ref().unwrap()).await;
    assert_eq!(second.state, S::Failed, "{second:?}");
    let error = second.error.unwrap();
    assert!(error.contains("hoster-a-b-c"), "{error}");
    assert!(error.contains("stack a-b"), "{error}");
    let volumes = h.runtime.volumes();
    assert_eq!(volumes.len(), 1);
    assert_eq!(volumes[0].labels[label::STACK], "a-b");
    assert_eq!(h.runtime.containers().len(), 1);
}

/// Deploys the stack-format example (with public stand-in images) against a real rootless
/// podman and the real registries. Run with `HOSTER_PODMAN_TESTS=1 cargo test`.
#[tokio::test]
async fn deploys_the_worked_example_on_real_podman() {
    use crate::registry::Anonymous;
    use crate::runtime::{PodmanRuntime, PodmanSocket};

    if !std::env::var("HOSTER_PODMAN_TESTS").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set HOSTER_PODMAN_TESTS=1 to deploy on rootless podman");
        return;
    }
    // The worked example's shape: a CI-built api under /api, a web frontend at /, and a
    // stop-first storage service with a volume on its own host.
    const EXAMPLE: &str = r#"
version = 1
name = "hostertest"

[services.api]
image = "ghcr.io/containerd/busybox"
command = ["httpd", "-f", "-p", "8080"]

[[services.api.routes]]
host = "notes.hoster.test"
prefix = "/api"
port = 8080

[services.web]
image = "docker.io/library/busybox:1.37"
command = ["httpd", "-f", "-p", "80"]

[[services.web.routes]]
host = "notes.hoster.test"
port = 80

[services.storage]
image = "docker.io/library/busybox:1.37"
command = ["sleep", "3600"]
update = "stop-first"

[services.storage.volumes]
data = "/data"
"#;
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(std::path::PathBuf::from);
    let runtime = Arc::new(
        PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
            .await
            .expect("connect to rootless podman"),
    );
    let registry = Arc::new(RegistryClient::new(Arc::new(Anonymous)).unwrap());
    let api_digest = registry
        .resolve(&"ghcr.io/containerd/busybox:1.36".parse().unwrap())
        .await
        .unwrap()
        .digest()
        .to_string();
    let orchestrator = Orchestrator::new(
        store.clone(),
        Arc::clone(&runtime),
        registry,
        PortAllocator::new(store.clone(), PortRange::try_from(28000..=28015).unwrap()),
        SecretStore::new(store.clone(), &crate::secrets::SecretKey::generate()),
        Settings::default(),
    );
    let applied = orchestrator.apply(EXAMPLE).await.unwrap();
    let first = orchestrator
        .wait(applied.deployment.unwrap().deployment)
        .await
        .unwrap();
    let deployed = orchestrator
        .deploy(
            "hostertest",
            DigestSet::from([("api".into(), api_digest)]),
            DeploymentSource::Manual,
        )
        .await
        .unwrap();
    let second = orchestrator.wait(deployed.deployment).await.unwrap();
    let kept = orchestrator
        .remove(RemoveStack::keeping_volumes("hostertest"))
        .await;
    assert_eq!(first.state, S::Live, "{first:?}");
    assert_eq!(second.state, S::Live, "{second:?}");
    let kept = kept.unwrap();
    assert_eq!(kept.containers, 3);
    assert!(kept.volumes.is_empty());

    // Re-apply (the volume is still there), then remove again deleting the volume.
    let applied = orchestrator.apply(EXAMPLE).await.unwrap();
    let again = orchestrator
        .wait(applied.deployment.unwrap().deployment)
        .await
        .unwrap();
    let deleted = orchestrator
        .remove(RemoveStack::deleting_volumes("hostertest", "hostertest"))
        .await;
    assert_eq!(again.state, S::Live, "{again:?}");
    assert_eq!(deleted.unwrap().volumes, ["hoster-hostertest-data"]);
    assert!(matches!(
        runtime.remove_volume("hoster-hostertest-data").await,
        Err(RuntimeError::NotFound {
            kind: ObjectKind::Volume,
            ..
        })
    ));
}

#[tokio::test]
async fn deploy_work_logs_inside_the_deployment_and_service_spans() {
    use crate::logging::LogFormat;
    use crate::logging::capture::subscriber;

    let h = harness().await;
    let (buffer, subscriber) = subscriber(LogFormat::Json, "hoster=info");
    // A current-thread test runtime: the spawned deploy task logs on this thread too.
    let guard = tracing::subscriber::set_default(subscriber);
    let applied = h.apply(NOTES).await;
    let outcome = applied.deployment.as_ref().unwrap();
    let deployment = h.finish(outcome).await;
    drop(guard);
    assert_eq!(deployment.state, S::Live, "{deployment:?}");

    let lines = buffer.json();
    let deployment_span = serde_json::json!({
        "name": "deployment",
        "stack": "notes",
        "deployment_id": outcome.deployment.to_string(),
        "generation": 1,
    });
    let live = lines
        .iter()
        .find(|l| l["target"] == "hoster::deploy" && l["to"] == "Live")
        .unwrap_or_else(|| panic!("no Live event in {lines:?}"));
    assert_eq!(live["spans"][0], deployment_span, "{live}");

    let started = lines
        .iter()
        .find(|l| l["message"] == "container started")
        .unwrap_or_else(|| panic!("no container start in {lines:?}"));
    assert_eq!(
        started["spans"],
        serde_json::json!([
            deployment_span,
            {"name": "service", "stack": "notes", "service": "storage", "generation": 1},
        ]),
        "{started}"
    );
}

/// Secrets, `PORT` and a volume on a real rootless podman: the app writes its environment and a
/// run counter kept in its volume to files it serves over HTTP. Run with
/// `HOSTER_PODMAN_TESTS=1 cargo test`.
#[tokio::test]
async fn secrets_port_and_volumes_on_real_podman() {
    use crate::registry::Anonymous;
    use crate::runtime::{PodmanRuntime, PodmanSocket};
    use std::io::{Read as _, Write as _};

    if !std::env::var("HOSTER_PODMAN_TESTS").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set HOSTER_PODMAN_TESTS=1 to deploy on rootless podman");
        return;
    }
    const SERVE: &str = "mkdir -p /www && echo run >> /data/runs && cp /data/runs /www/runs \
                         && env > /www/env && exec httpd -f -p 8080 -h /www";
    let stack = format!(
        r#"
version = 1
name = "hostersecrets"

[services.app]
image = "docker.io/library/busybox:1.37"
command = ["sh", "-c", "{SERVE}"]
update = "stop-first"
secrets = {{ APP_TOKEN = "hostersecrets/token" }}

[services.app.volumes]
data = "/data"

[[services.app.routes]]
host = "app.hoster.test"
port = 8080

[services.multi]
image = "docker.io/library/busybox:1.37"
command = ["sh", "-c", "mkdir -p /www && env > /www/env && exec httpd -f -p 8080 -h /www"]

[[services.multi.routes]]
host = "multi.hoster.test"
port = 8080

[[services.multi.routes]]
host = "multi.hoster.test"
prefix = "/admin"
port = 9090
"#
    );
    let get = |port: u16, path: &str| -> String {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(stream, "GET {path} HTTP/1.0\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    };
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(std::path::PathBuf::from);
    let runtime = Arc::new(
        PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
            .await
            .expect("connect to rootless podman"),
    );
    let secrets = SecretStore::new(store.clone(), &SecretKey::generate());
    let token: SecretName = "hostersecrets/token".parse().unwrap();
    secrets.set(&token, &"first-token".into()).await.unwrap();
    let orchestrator = Orchestrator::new(
        store.clone(),
        Arc::clone(&runtime),
        Arc::new(RegistryClient::new(Arc::new(Anonymous)).unwrap()),
        PortAllocator::new(store.clone(), PortRange::try_from(28100..=28115).unwrap()),
        secrets.clone(),
        Settings::default(),
    );
    let port_of = |pointers: &[crate::store::ServicePointer], service: &str, container: u16| {
        pointers
            .iter()
            .find(|p| p.service == service)
            .unwrap()
            .ports[&container]
    };

    let applied = orchestrator.apply(&stack).await.unwrap();
    let first = orchestrator
        .wait(applied.deployment.unwrap().deployment)
        .await
        .unwrap();
    let pointers = store.services("hostersecrets").await.unwrap();
    let app_env = get(port_of(&pointers, "app", 8080), "/env");
    let multi_env = get(port_of(&pointers, "multi", 8080), "/env");

    // A replaced secret redeploys app; its volume keeps the first run's data.
    secrets.set(&token, &"second-token".into()).await.unwrap();
    let redeployed = orchestrator
        .deploy("hostersecrets", DigestSet::new(), DeploymentSource::Manual)
        .await
        .unwrap();
    let second = orchestrator.wait(redeployed.deployment).await.unwrap();
    let pointers = store.services("hostersecrets").await.unwrap();
    let app_env_2 = get(port_of(&pointers, "app", 8080), "/env");
    let runs = get(port_of(&pointers, "app", 8080), "/runs");
    let removed = orchestrator
        .remove(RemoveStack::keeping_volumes("hostersecrets"))
        .await;
    let volume_kept = std::process::Command::new("podman")
        .args(["volume", "exists", "hoster-hostersecrets-data"])
        .status()
        .is_ok_and(|s| s.success());
    let _ = std::process::Command::new("podman")
        .args(["volume", "rm", "--force", "hoster-hostersecrets-data"])
        .status();

    assert_eq!(first.state, S::Live, "{first:?}");
    assert_eq!(second.state, S::Live, "{second:?}");
    assert!(app_env.contains("APP_TOKEN=first-token"), "{app_env}");
    assert!(app_env.contains("PORT=8080"), "{app_env}");
    assert!(!multi_env.contains("PORT="), "{multi_env}");
    assert!(app_env_2.contains("APP_TOKEN=second-token"), "{app_env_2}");
    assert_eq!(runs.matches("run").count(), 2, "{runs}");
    assert_eq!(removed.unwrap().containers, 2);
    assert!(volume_kept, "the volume survives stack removal");
}
