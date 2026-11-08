//! Scenario tests: the orchestrator against [`FakeRuntime`] and a real (temporary) store.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;

use super::*;
use crate::config::PortRange;
use crate::runtime::{ContainerInfo, ContainerState, FakeRuntime, Operation};
use crate::stack::ImageRef;

const NOTES: &str = r#"
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

fn digest(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

/// Resolves every tag to `digest(0xaa)` and counts the lookups.
#[derive(Default)]
struct FakeResolver {
    calls: AtomicUsize,
}

#[async_trait]
impl TagResolver for FakeResolver {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(image.clone().pin(digest(0xaa).parse().unwrap()))
    }
}

struct Harness {
    _dir: TempDir,
    store: Store,
    runtime: Arc<FakeRuntime>,
    resolver: Arc<FakeResolver>,
    orchestrator: Arc<Orchestrator<FakeRuntime>>,
}

async fn harness_with(settle: Duration) -> Harness {
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
    let orchestrator = Orchestrator::new(
        store.clone(),
        Arc::clone(&runtime),
        Arc::clone(&resolver) as Arc<dyn TagResolver>,
        ports,
        settings,
    );
    Harness {
        _dir: dir,
        store,
        runtime,
        resolver,
        orchestrator,
    }
}

async fn harness() -> Harness {
    harness_with(Duration::from_millis(20)).await
}

impl Harness {
    async fn apply(&self, toml: &str) -> ApplyOutcome {
        self.orchestrator.apply(toml).await.unwrap()
    }

    async fn deploy(&self, images: &[(&str, u8)]) -> DeployOutcome {
        let images = images
            .iter()
            .map(|(s, d)| ((*s).to_owned(), digest(*d)))
            .collect();
        self.orchestrator
            .deploy("notes", images, DeploymentSource::Webhook)
            .await
            .unwrap()
    }

    async fn finish(&self, outcome: &DeployOutcome) -> Deployment {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.orchestrator.wait(outcome.deployment),
        )
        .await
        .expect("deployment finishes")
        .unwrap()
    }

    /// Applies NOTES (storage deploys) and deploys api and web: every service at a digest.
    async fn bootstrap(&self) {
        let applied = self.apply(NOTES).await;
        let first = self.finish(applied.deployment.as_ref().unwrap()).await;
        assert_eq!(first.state, DeploymentState::Live, "{first:?}");
        let deployed = self.deploy(&[("api", 1), ("web", 1)]).await;
        let second = self.finish(&deployed).await;
        assert_eq!(second.state, DeploymentState::Live, "{second:?}");
    }

    fn container_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .runtime
            .containers()
            .into_iter()
            .map(|c| c.name)
            .collect();
        names.sort();
        names
    }

    fn container(&self, name: &str) -> ContainerInfo {
        self.runtime
            .containers()
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no container {name}"))
    }

    async fn generations(&self) -> Vec<(String, u32)> {
        self.store
            .services("notes")
            .await
            .unwrap()
            .into_iter()
            .map(|p| (p.service, p.generation))
            .collect()
    }

    async fn states(&self, id: DeploymentId) -> Vec<DeploymentState> {
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
async fn remove_deletes_containers_network_and_records() {
    let h = harness().await;
    h.bootstrap().await;
    let removed = h.orchestrator.remove("notes").await.unwrap();
    assert_eq!(removed.containers, 3);
    assert!(h.runtime.containers().is_empty());
    assert!(h.runtime.networks().is_empty());
    assert!(h.store.stack("notes").await.unwrap().is_none());
    assert!(h.store.port_allocations().await.unwrap().is_empty());
    assert!(matches!(
        h.orchestrator.remove("notes").await.unwrap_err(),
        OrchestratorError::UnknownStack(_)
    ));
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
    let removed = orchestrator.remove("hostertest").await;
    assert_eq!(first.state, S::Live, "{first:?}");
    assert_eq!(second.state, S::Live, "{second:?}");
    assert_eq!(removed.unwrap().containers, 3);
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
