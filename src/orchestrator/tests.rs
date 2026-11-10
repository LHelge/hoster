//! Scenario tests: the orchestrator against [`FakeRuntime`] and a real (temporary) store.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;

use super::*;
use crate::config::PortRange;
use crate::health::Check;
use crate::runtime::{ContainerInfo, ContainerState, ExecOutcome, FakeRuntime, Operation};
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

/// Resolves every tag to `digest(<digest>)`, `digest(0xaa)` unless changed, and counts the
/// lookups. Every repository holds every digest, except `digest(0xee)`, which none holds, and
/// `digest(0xef)`, whose lookup fails.
pub(super) struct FakeResolver {
    calls: AtomicUsize,
    /// What tags resolve to; change it to move a tag upstream.
    pub(super) digest: std::sync::atomic::AtomicU8,
    /// The (image, digest) pairs looked up with `contains`.
    pub(super) lookups: std::sync::Mutex<Vec<(String, String)>>,
}

impl Default for FakeResolver {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            digest: std::sync::atomic::AtomicU8::new(0xaa),
            lookups: std::sync::Mutex::default(),
        }
    }
}

#[async_trait]
impl TagResolver for FakeResolver {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let to = digest(self.digest.load(Ordering::SeqCst));
        Ok(image.clone().pin(to.parse().unwrap()))
    }

    async fn contains(&self, image: &ImageRef, found: &Digest) -> Result<(), RegistryError> {
        self.lookups
            .lock()
            .unwrap()
            .push((image.to_string(), found.to_string()));
        if found.as_ref() == digest(0xee) {
            Err(RegistryError::NotFound {
                image: format!("{image}@{found}"),
            })
        } else if found.as_ref() == digest(0xef) {
            Err(RegistryError::Status {
                registry: image.registry().to_owned(),
                what: "the manifest request",
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            })
        } else {
            Ok(())
        }
    }
}

/// Answers probes by container port: a scripted result, `HTTP 200` otherwise, after `delay`.
/// Records every probe. Probes on a held port never answer, as if the deploy task were frozen
/// there (the "crash" of the recovery scenarios).
#[derive(Default)]
pub(super) struct FakeProber {
    delay: Duration,
    answers: StdMutex<BTreeMap<u16, Result<String, String>>>,
    probes: StdMutex<Vec<String>>,
    held: StdMutex<BTreeSet<u16>>,
}

impl FakeProber {
    /// A prober whose every probe takes `delay`.
    pub(super) fn with_delay(delay: Duration) -> Self {
        Self {
            delay,
            ..Self::default()
        }
    }

    pub(super) fn answer(&self, port: u16, result: Result<&str, &str>) {
        let result = result.map(str::to_owned).map_err(str::to_owned);
        self.answers.lock().unwrap().insert(port, result);
    }

    /// From now on, probes on container port `port` do not answer until it is
    /// [`release`](Self::release)d. The probes made on it so far are forgotten, so
    /// [`probed`](Self::probed) waits for one that is held, not for one from an earlier deploy.
    pub(super) fn hold(&self, port: u16) {
        let pattern = Self::pattern(port);
        self.probes
            .lock()
            .unwrap()
            .retain(|p| !p.contains(&pattern));
        self.held.lock().unwrap().insert(port);
    }

    /// Probes on container port `port`, held or not, answer again.
    pub(super) fn release(&self, port: u16) {
        self.held.lock().unwrap().remove(&port);
    }

    fn is_held(&self, text: &str) -> bool {
        self.held
            .lock()
            .unwrap()
            .iter()
            .any(|&port| text.contains(&Self::pattern(port)))
    }

    /// The probes so far, sorted, forgetting them.
    pub(super) fn take_probes(&self) -> Vec<String> {
        let mut probes = std::mem::take(&mut *self.probes.lock().unwrap());
        probes.sort();
        probes
    }

    /// Waits until a probe on container port `port` has been made.
    pub(super) async fn probed(&self, port: u16) {
        let pattern = Self::pattern(port);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self
                .probes
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.contains(&pattern))
            {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("probed within 10 s");
    }

    fn pattern(port: u16) -> String {
        format!("container port {port} ")
    }
}

#[async_trait]
impl Prober for FakeProber {
    async fn probe(&self, check: &Check) -> std::result::Result<String, String> {
        let text = check.to_string();
        self.probes.lock().unwrap().push(text.clone());
        while self.is_held(&text) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        tokio::time::sleep(self.delay).await;
        self.answers
            .lock()
            .unwrap()
            .iter()
            .find(|(port, _)| text.contains(&Self::pattern(**port)))
            .map_or_else(|| Ok("HTTP 200".to_owned()), |(_, r)| r.clone())
    }
}

pub(super) struct Harness {
    pub(super) _dir: TempDir,
    pub(super) store: Store,
    pub(super) runtime: Arc<FakeRuntime>,
    pub(super) resolver: Arc<FakeResolver>,
    pub(super) secrets: SecretStore,
    pub(super) orchestrator: Arc<Orchestrator<FakeRuntime>>,
    /// The table the orchestrator publishes, as the proxy reads it.
    pub(super) routes: Arc<ArcSwap<RouteTable>>,
    /// Answers the health probes of deployments (and of the health monitor).
    pub(super) prober: Arc<FakeProber>,
}

/// Settings for tests: health checks and drains take a few milliseconds.
pub(super) fn settings(settle: Duration) -> Settings {
    Settings {
        settle,
        settle_poll: Duration::from_millis(5),
        drain_settle: Duration::from_millis(5),
        ..Settings::default()
    }
}

/// A harness whose health checks take `settle`: every probe answers after it, and a service
/// without routes must stay up that long.
pub(super) async fn harness_with(settle: Duration) -> Harness {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let runtime = Arc::new(FakeRuntime::new());
    let resolver = Arc::new(FakeResolver::default());
    let ports = PortAllocator::new(store.clone(), PortRange::try_from(20000..=20015).unwrap());
    let secrets = SecretStore::new(store.clone(), &SecretKey::generate());
    let routes = Arc::<ArcSwap<RouteTable>>::default();
    let prober = Arc::new(FakeProber::with_delay(settle));
    // The host's `/proc` and `/sys` are fixtures (ADR-0059 §5); none until a test writes them.
    let settings = Settings {
        machine: Machine::under(&dir.path().join("host"), 1000),
        ..settings(settle)
    };
    let orchestrator = Orchestrator::with_prober(
        store.clone(),
        Arc::clone(&runtime),
        Arc::clone(&resolver) as Arc<dyn TagResolver>,
        ports,
        secrets.clone(),
        settings,
        Arc::clone(&routes),
        Arc::clone(&prober) as Arc<dyn Prober>,
    );
    Harness {
        _dir: dir,
        store,
        runtime,
        resolver,
        secrets,
        orchestrator,
        routes,
        prober,
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

    /// The states deployment `id` went through; notes (probe results, drain timeouts) are
    /// events within one state and left out.
    pub(super) async fn states(&self, id: DeploymentId) -> Vec<DeploymentState> {
        self.store
            .events(id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.from != Some(e.to))
            .map(|e| e.to)
            .collect()
    }

    /// The notes recorded on deployment `id`, with the state they were made in.
    pub(super) async fn notes(&self, id: DeploymentId) -> Vec<(DeploymentState, String)> {
        self.store
            .events(id)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.from == Some(e.to))
            .map(|e| (e.to, e.message.unwrap_or_default()))
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
    assert!(events.windows(2).all(|w| w[0].at <= w[1].at), "{events:#?}");
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

/// The editor's live check (ADR-0060): every local check, with the line of each problem, and
/// never a registry call.
#[tokio::test]
async fn check_file_reports_local_problems_with_their_lines_and_never_resolves_tags() {
    let h = harness().await;
    let clean = h.orchestrator.check_file(NOTES).await.unwrap();
    assert_eq!(clean, CheckReport::default());

    // Parse and validation errors keep their own locations.
    let invalid = h
        .orchestrator
        .check_file("version = 1\nname = \"Bad\"\n\n[services.a]\nimage = \"ghcr.io/acme/a\"\n")
        .await
        .unwrap();
    assert!(invalid.warnings.is_empty());
    assert!(
        invalid
            .errors
            .iter()
            .any(|d| d.path.to_string() == "name" && d.location.is_some_and(|l| l.line == 2)),
        "{invalid:?}"
    );

    // A route another stack owns is an error at that route's `host`.
    h.apply(NOTES).await;
    let other = "version = 1\nname = \"other\"\n\n[services.app]\nimage = \"ghcr.io/acme/other\"\n\n[[services.app.routes]]\nhost = \"own.example.com\"\nport = 80\n\n[[services.app.routes]]\nhost = \"notes.example.com\"\nprefix = \"/api\"\nport = 80\n";
    let collided = h.orchestrator.check_file(other).await.unwrap();
    assert_eq!(collided.errors.len(), 1, "{collided:?}");
    let collision = &collided.errors[0];
    assert_eq!(collision.path.to_string(), "services.app.routes[1].host");
    assert_eq!(
        collision.location.map(|l| (l.line, l.column)),
        Some((12, 1))
    );
    assert!(collision.message.contains("notes"), "{}", collision.message);

    // A secret that is not set yet warns at its key; once set, the warning goes.
    let unset = h.orchestrator.check_file(WITH_SECRETS).await.unwrap();
    assert!(unset.errors.is_empty(), "{unset:?}");
    assert_eq!(unset.warnings.len(), 1, "{unset:?}");
    let warning = &unset.warnings[0];
    assert_eq!(
        warning.path.to_string(),
        "services.api.secrets.DATABASE_URL"
    );
    assert_eq!(warning.location.map(|l| (l.line, l.column)), Some((8, 13)));
    assert!(
        warning.message.contains("notes/database-url"),
        "{}",
        warning.message
    );
    h.set_secret("notes/database-url", "postgres://x").await;
    let set = h.orchestrator.check_file(WITH_SECRETS).await.unwrap();
    assert_eq!(set, CheckReport::default());

    // Only the apply above resolved a tag (storage's); the checks resolved none.
    assert_eq!(h.resolver.calls.load(Ordering::SeqCst), 1);
}

/// The preview lists the order the apply would start the services in (ADR-0056 Decision 2).
#[tokio::test]
async fn preview_lists_the_start_order() {
    let h = harness().await;
    let name = |n: &str| n.parse::<Name>().unwrap();
    let preview = h.orchestrator.preview(DEPS).await.unwrap();
    assert_eq!(
        preview.start_order,
        [
            Wave::Start(BTreeMap::from([(name("db"), Vec::new())])),
            Wave::Start(BTreeMap::from([(name("app"), vec![name("db")])])),
        ]
    );
    // Once both run, a changed db is replaced and app, changed too, is held back for it.
    let applied = h.apply(DEPS).await;
    h.finish(applied.deployment.as_ref().unwrap()).await;
    let both = DEPS
        .replace("app:1\"", "app:1\"\nenv = { A = \"1\" }")
        .replace("postgres:17\"", "postgres:17\"\nenv = { A = \"1\" }");
    let preview = h.orchestrator.preview(&both).await.unwrap();
    assert_eq!(
        preview.start_order,
        [
            Wave::Replace(name("db")),
            Wave::HeldBack {
                service: name("app"),
                behind: vec![name("db")],
            },
        ]
    );
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
async fn every_persisted_transition_reaches_ui_events() {
    let h = harness().await;
    let mut ui = h.orchestrator.subscribe();
    let applied = h.apply(NOTES).await;
    let outcome = applied.deployment.unwrap();
    h.finish(&outcome).await;
    let mut states = Vec::new();
    while let Ok(event) = ui.try_recv() {
        if let UiEvent::Deployment { stack, id, state } = event {
            assert_eq!((stack.as_str(), id), ("notes", outcome.deployment));
            if states.last() != Some(&state) {
                states.push(state);
            }
        }
    }
    let persisted = h.states(outcome.deployment).await;
    // The first event, `Received`, is written when the deployment is created; no page can be
    // following it yet.
    assert_eq!(states, persisted[1..], "{states:?} vs {persisted:?}");
    assert_eq!(states.last(), Some(&S::Live));
}

#[tokio::test]
async fn history_lists_a_stacks_deployments_newest_first() {
    let h = harness().await;
    h.bootstrap().await;
    let history = h.orchestrator.history("notes", 10).await.unwrap();
    assert_eq!(history.len(), 2);
    assert!(history[0].id > history[1].id);
    assert_eq!(history[0].source, DeploymentSource::Webhook);
    assert_eq!(h.orchestrator.history("notes", 1).await.unwrap().len(), 1);
    assert!(matches!(
        h.orchestrator.history("other", 10).await,
        Err(OrchestratorError::UnknownStack(_))
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
    // Crash the new container while its health check watches it.
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
pub(super) const WITH_SECRETS: &str = r#"
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
    pub(super) async fn set_secret(&self, name: &str, value: &str) {
        self.secrets
            .set(&name.parse::<SecretName>().unwrap(), &value.into())
            .await
            .unwrap();
    }

    pub(super) fn spec_of(&self, name: &str) -> crate::runtime::ContainerSpec {
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
/// podman and the real registries. Run with `TEST_HOSTER_PODMAN=1 cargo test`.
#[tokio::test]
async fn deploys_the_worked_example_on_real_podman() {
    use crate::registry::Anonymous;
    use crate::runtime::{PodmanRuntime, PodmanSocket};

    if !std::env::var("TEST_HOSTER_PODMAN").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to deploy on rootless podman");
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

# busybox httpd serves no /healthz, the default probe (ADR-0051).
[services.api.health]
tcp = { port = 8080 }

[[services.api.routes]]
host = "notes.hoster.test"
prefix = "/api"
port = 8080

[services.web]
image = "docker.io/library/busybox:1.37"
command = ["httpd", "-f", "-p", "80"]

[services.web.health]
tcp = { port = 80 }

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
        Arc::default(),
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
/// `TEST_HOSTER_PODMAN=1 cargo test`.
#[tokio::test]
async fn secrets_port_and_volumes_on_real_podman() {
    use crate::registry::Anonymous;
    use crate::runtime::{PodmanRuntime, PodmanSocket};
    use std::io::{Read as _, Write as _};

    if !std::env::var("TEST_HOSTER_PODMAN").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to deploy on rootless podman");
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

# httpd serves /www, which has no /healthz (the default probe, ADR-0051).
[services.app.health]
http = {{ path = "/env", port = 8080 }}

[[services.app.routes]]
host = "app.hoster.test"
port = 8080

[services.multi]
image = "docker.io/library/busybox:1.37"
command = ["sh", "-c", "mkdir -p /www && env > /www/env && exec httpd -f -p 8080 -h /www"]

[services.multi.health]
http = {{ path = "/env", port = 8080 }}

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
        Arc::default(),
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

/// Exec health checks on a real rootless podman (ADR-0056): `db` (no routes) becomes ready a
/// moment after it starts, `app` (routed) also needs its port; the monitor runs the same checks
/// in the live containers, and a check podman cannot run fails a deploy at once. Run with
/// `TEST_HOSTER_PODMAN=1 cargo test`.
#[tokio::test]
async fn exec_checks_on_real_podman() {
    use crate::registry::Anonymous;
    use crate::runtime::{PodmanRuntime, PodmanSocket};

    if !std::env::var("TEST_HOSTER_PODMAN").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to deploy on rootless podman");
        return;
    }
    let stack = |db_check: &str| {
        format!(
            r#"
version = 1
name = "hosterexecrxt"

[services.db]
image = "docker.io/library/busybox:1.37"
command = ["sh", "-c", "sleep 2 && touch /tmp/ready && exec sleep 3600"]
stop_timeout = "1s"

[services.db.health]
exec = {db_check}
interval = "500ms"
start_period = "20s"

[services.app]
image = "docker.io/library/busybox:1.37"
command = ["sh", "-c", "mkdir -p /www && exec httpd -f -p 8080 -h /www"]
stop_timeout = "1s"

[services.app.health]
exec = ["true"]

[[services.app.routes]]
host = "app.hoster.test"
port = 8080
"#
        )
    };
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(std::path::PathBuf::from);
    let runtime = Arc::new(
        PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
            .await
            .expect("connect to rootless podman"),
    );
    let orchestrator = Orchestrator::new(
        store.clone(),
        Arc::clone(&runtime),
        Arc::new(RegistryClient::new(Arc::new(Anonymous)).unwrap()),
        PortAllocator::new(store.clone(), PortRange::try_from(28120..=28135).unwrap()),
        SecretStore::new(store.clone(), &SecretKey::generate()),
        Settings::default(),
        Arc::default(),
    );

    let started = tokio::time::Instant::now();
    let applied = orchestrator
        .apply(&stack(r#"["sh", "-c", "test -f /tmp/ready"]"#))
        .await
        .unwrap();
    let first = orchestrator
        .wait(applied.deployment.unwrap().deployment)
        .await
        .unwrap();
    let took = started.elapsed();
    let first_notes: Vec<String> = store
        .events(first.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| e.message)
        .collect();
    let live = orchestrator
        .monitor_at(tokio::time::Instant::now())
        .await
        .unwrap();

    let started = tokio::time::Instant::now();
    let broken = orchestrator
        .apply(&stack(r#"["pg_isready", "-U", "postgres"]"#))
        .await
        .unwrap();
    let second = orchestrator
        .wait(broken.deployment.unwrap().deployment)
        .await
        .unwrap();
    let failed_after = started.elapsed();
    let removed = orchestrator
        .remove(RemoveStack::keeping_volumes("hosterexecrxt"))
        .await;

    assert_eq!(first.state, S::Live, "{first:?} {first_notes:#?}");
    assert!(
        took >= Duration::from_secs(2),
        "db became ready after 2 s: {took:?}"
    );
    let has = |text: &str| first_notes.iter().any(|n| n.contains(text));
    assert!(
        has(r#"checking db with exec: sh -c "test -f /tmp/ready", declared in health.exec"#),
        "{first_notes:#?}"
    );
    assert!(
        has("health probe exec: sh -c \"test -f /tmp/ready\": exit 1"),
        "{first_notes:#?}"
    );
    assert!(
        has("health check exec: true and TCP connect to container port 8080"),
        "{first_notes:#?}"
    );
    let mut healthy: Vec<_> = live
        .iter()
        .filter_map(|e| match e {
            UiEvent::Health {
                service,
                to,
                detail,
                ..
            } => Some((service.as_str(), *to, detail.as_str())),
            _ => None,
        })
        .collect();
    healthy.sort_by_key(|h| h.0);
    assert_eq!(
        healthy,
        [
            ("app", HealthStatus::Healthy, "exit 0; TCP: connected"),
            ("db", HealthStatus::Healthy, "exit 0"),
        ]
    );
    assert_eq!(second.state, S::Failed, "{second:?}");
    let error = second.error.as_deref().unwrap();
    assert!(
        error.contains("`health.exec` cannot run: `pg_isready`: executable file not found"),
        "{error}"
    );
    assert!(
        failed_after < Duration::from_secs(20),
        "at once: {failed_after:?}"
    );
    assert_eq!(removed.unwrap().containers, 2);
}

#[tokio::test]
async fn apply_ensures_certificates_before_deploying_and_remove_forgets_them() {
    use crate::acme::{AcmeCommand, AcmeEvent};
    use tokio::sync::mpsc;

    let h = harness().await;
    let (acme_tx, mut acme_rx) = mpsc::unbounded_channel();
    h.orchestrator.connect_acme(acme_tx);
    assert!(acme_rx.try_recv().is_err(), "no stack routes anything yet");

    let applied = h.apply(NOTES).await;
    // Sent at apply, whatever the deployment does next (ADR-0036).
    assert_eq!(
        acme_rx.try_recv().unwrap(),
        AcmeCommand::Ensure {
            hosts: vec!["notes.example.com".into(), "s3.notes.example.com".into()]
        }
    );
    h.finish(applied.deployment.as_ref().unwrap()).await;

    // The ACME task's events are recorded and relayed to the UI.
    let mut ui = h.orchestrator.subscribe();
    let (events_tx, events_rx) = mpsc::channel(4);
    let relay = tokio::spawn(Arc::clone(&h.orchestrator).run_acme_events(events_rx));
    let failed = AcmeEvent::Failed {
        host: "notes.example.com".into(),
        error: "the CA could not validate the HTTP-01 challenge".into(),
        retry_at: std::time::SystemTime::UNIX_EPOCH,
    };
    events_tx.send(failed.clone()).await.unwrap();
    drop(events_tx);
    relay.await.unwrap();
    // The event log's line is stored and announced before the certificate status is relayed.
    let UiEvent::Event(line) = ui.try_recv().unwrap() else {
        panic!("the failure is not in the event log");
    };
    assert_eq!(line.kind, crate::store::EventKind::CertificateFailed);
    assert_eq!(line.stack.as_deref(), Some("notes"));
    assert_eq!(line.subject.as_deref(), Some("/certificates"));
    let stored = h
        .store
        .event_page(&crate::store::EventQuery::default())
        .await
        .unwrap();
    assert!(stored.events.contains(&line), "{stored:?}");
    assert_eq!(ui.try_recv().unwrap(), UiEvent::Certificate(failed));
    assert_eq!(
        h.orchestrator.certificates()["notes.example.com"]
            .error
            .as_deref(),
        Some("the CA could not validate the HTTP-01 challenge")
    );

    h.orchestrator
        .remove(RemoveStack::keeping_volumes("notes"))
        .await
        .unwrap();
    let forgotten: Vec<_> = std::iter::from_fn(|| acme_rx.try_recv().ok()).collect();
    assert_eq!(
        forgotten,
        ["notes.example.com", "s3.notes.example.com"]
            .map(|host| AcmeCommand::Forget { host: host.into() })
    );
    assert!(h.orchestrator.certificates().is_empty());
}

#[tokio::test]
async fn rollback_and_restart_follow_the_active_revisions_hosts() {
    use crate::acme::AcmeCommand;
    use tokio::sync::mpsc;

    fn drain(rx: &mut mpsc::UnboundedReceiver<AcmeCommand>) -> Vec<AcmeCommand> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }
    let ensure = |hosts: &[&str]| AcmeCommand::Ensure {
        hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
    };
    let forget = |host: &str| AcmeCommand::Forget { host: host.into() };

    let h = harness().await;
    h.bootstrap().await;
    let good = h.store.deployments("notes", 1).await.unwrap().remove(0);
    let (acme_tx, mut acme_rx) = mpsc::unbounded_channel();
    h.orchestrator.connect_acme(acme_tx);
    let moved = NOTES.replace("s3.notes.example.com", "s3.example.org");
    h.finish(h.apply(&moved).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(
        drain(&mut acme_rx),
        [
            ensure(&["notes.example.com", "s3.notes.example.com"]),
            ensure(&["notes.example.com", "s3.example.org"]),
            forget("s3.notes.example.com"),
        ]
    );

    // Rolling back makes the old revision, and its hosts, the active ones again.
    let rolled = h.orchestrator.rollback("notes", good.id).await.unwrap();
    assert_eq!(
        drain(&mut acme_rx),
        [
            ensure(&["notes.example.com", "s3.notes.example.com"]),
            forget("s3.example.org"),
        ]
    );
    h.finish(&rolled).await;

    // A revision whose deployment fails still has its hosts ensured, at apply and after a
    // restart alike (ADR-0036).
    h.runtime.fail_next(
        Operation::PullImage,
        RuntimeError::Api {
            operation: Operation::PullImage,
            message: "boom".into(),
        },
    );
    let failed = h
        .finish(h.apply(&moved).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(failed.state, S::Failed);
    drain(&mut acme_rx);
    let restarted = Orchestrator::new(
        h.store.clone(),
        Arc::clone(&h.runtime),
        Arc::clone(&h.resolver) as Arc<dyn TagResolver>,
        PortAllocator::new(h.store.clone(), PortRange::try_from(20000..=20015).unwrap()),
        h.secrets.clone(),
        Settings::default(),
        Arc::default(),
    );
    let (acme_tx, mut acme_rx) = mpsc::unbounded_channel();
    restarted.connect_acme(acme_tx);
    restarted.recover().await.unwrap();
    restarted.restore_routes().await.unwrap();
    assert_eq!(
        drain(&mut acme_rx),
        [ensure(&["notes.example.com", "s3.example.org"])]
    );
}

// ---------------------------------------------------------------------------------------------
// The deploy webhook (ADR-0042, ADR-0047)
// ---------------------------------------------------------------------------------------------

/// NOTES with a `[deploy]` rule: repository 42, tags `v*` only.
pub(super) fn hooked() -> String {
    format!("{NOTES}\n[deploy]\nrepository_id = 42\nrefs = [\"refs/tags/v*\"]\n")
}

pub(super) fn caller(repository_id: u64, git_ref: &str) -> DeployCaller {
    DeployCaller {
        repository_id,
        git_ref: git_ref.into(),
        environment: None,
        job_workflow_ref: None,
    }
}

pub(super) fn provenance() -> Provenance {
    Provenance {
        repository: "acme/notes".into(),
        sha: "0f3c5e1a9d2b7c4e8f6a1b3d5c7e9f0a2b4c6d8e".into(),
        git_ref: "refs/tags/v1.4.2".into(),
        run_id: "123456".into(),
        actor: "octocat".into(),
        tag: Some("v1.4.2".into()),
    }
}

pub(super) fn webhook(stack: &str, images: &[(&str, u8)], caller: DeployCaller) -> WebhookDeploy {
    WebhookDeploy {
        stack: stack.into(),
        images: images
            .iter()
            .map(|(s, d)| ((*s).to_owned(), digest(*d)))
            .collect(),
        caller,
        provenance: provenance(),
    }
}

#[tokio::test]
async fn webhook_deploys_several_services_as_one_deployment_with_provenance() {
    let h = harness().await;
    h.finish(h.apply(&hooked()).await.deployment.as_ref().unwrap())
        .await;
    let outcome = h
        .orchestrator
        .deploy_webhook(webhook(
            "notes",
            &[("api", 1), ("web", 1)],
            caller(42, "refs/tags/v1.4.2"),
        ))
        .await
        .unwrap();
    let deployment = h.finish(&outcome).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    assert_eq!(deployment.source, DeploymentSource::Webhook);
    assert_eq!(deployment.provenance, Some(provenance()));
    assert_eq!(deployment.target["api"], digest(1));
    assert_eq!(deployment.target["web"], digest(1));
    // Each digest was looked up in its own service's repository.
    let mut lookups = h.resolver.lookups.lock().unwrap().clone();
    lookups.sort();
    assert_eq!(
        lookups,
        [
            ("ghcr.io/acme/notes-api".to_owned(), digest(1)),
            ("ghcr.io/acme/notes-web".to_owned(), digest(1)),
        ]
    );
    // The same digests again: nothing to do.
    let again = h
        .orchestrator
        .deploy_webhook(webhook(
            "notes",
            &[("api", 1), ("web", 1)],
            caller(42, "refs/tags/v1.4.2"),
        ))
        .await
        .unwrap();
    assert_eq!(again.state, S::Noop);
}

#[tokio::test]
async fn webhook_is_forbidden_without_a_matching_deploy_rule() {
    let h = harness().await;
    let forbidden = |err: OrchestratorError| match err {
        OrchestratorError::Forbidden { reason, .. } => reason,
        other => panic!("expected Forbidden, got {other}"),
    };
    // An unknown stack looks the same as a refused one.
    let reason = forbidden(
        h.orchestrator
            .deploy_webhook(webhook("ghost", &[("api", 1)], caller(42, "refs/tags/v1")))
            .await
            .unwrap_err(),
    );
    assert!(reason.contains("does not exist"), "{reason}");
    // A stack without `[deploy]` takes no webhook deploys.
    h.apply(NOTES).await;
    let reason = forbidden(
        h.orchestrator
            .deploy_webhook(webhook("notes", &[("api", 1)], caller(42, "refs/tags/v1")))
            .await
            .unwrap_err(),
    );
    assert!(reason.contains("[deploy]"), "{reason}");
    h.apply(&hooked()).await;
    let reason = forbidden(
        h.orchestrator
            .deploy_webhook(webhook("notes", &[("api", 1)], caller(7, "refs/tags/v1")))
            .await
            .unwrap_err(),
    );
    assert!(reason.contains("repository_id 7"), "{reason}");
    let reason = forbidden(
        h.orchestrator
            .deploy_webhook(webhook(
                "notes",
                &[("api", 1)],
                caller(42, "refs/heads/main"),
            ))
            .await
            .unwrap_err(),
    );
    assert!(reason.contains("refs/heads/main"), "{reason}");
    // Nothing was looked up or recorded for a refused caller.
    assert!(h.resolver.lookups.lock().unwrap().is_empty());
    let deployments = h.store.deployments("notes", 10).await.unwrap();
    assert!(
        deployments
            .iter()
            .all(|d| d.source != DeploymentSource::Webhook)
    );
}

#[tokio::test]
async fn webhook_payload_checks_and_digest_ownership() {
    let h = harness().await;
    h.apply(&hooked()).await;
    let ok = || caller(42, "refs/tags/v1");
    let deploy = |images: &[(&str, u8)]| {
        h.orchestrator
            .deploy_webhook(webhook("notes", images, ok()))
    };
    assert!(matches!(
        deploy(&[("db", 1)]).await.unwrap_err(),
        OrchestratorError::UnknownService(_)
    ));
    assert!(matches!(
        deploy(&[("storage", 1)]).await.unwrap_err(),
        OrchestratorError::NotCiBuilt(_)
    ));
    // A digest the service's repository does not hold, even with a valid one beside it.
    let err = deploy(&[("api", 1), ("web", 0xee)]).await.unwrap_err();
    match &err {
        OrchestratorError::ForeignDigest {
            service,
            repository,
        } => {
            assert_eq!(service, "web");
            assert_eq!(repository, "ghcr.io/acme/notes-web");
        }
        other => panic!("expected ForeignDigest, got {other}"),
    }
    assert!(matches!(
        deploy(&[("api", 0xef)]).await.unwrap_err(),
        OrchestratorError::Resolve { .. }
    ));
    let mut request = webhook("notes", &[], ok());
    request.images.insert("api".into(), "latest".into());
    assert!(matches!(
        h.orchestrator.deploy_webhook(request).await.unwrap_err(),
        OrchestratorError::Digest { .. }
    ));
}

#[tokio::test]
async fn refused_webhook_attempts_show_in_the_stack_status() {
    let h = harness().await;
    h.apply(NOTES).await;
    let rejection = WebhookRejection {
        stack: "notes".into(),
        reason: "no [deploy] section".into(),
        repository: "acme/notes".into(),
        repository_id: "42".into(),
        git_ref: "refs/tags/v1".into(),
        environment: Some("production".into()),
        workflow_ref: None,
        job_workflow_ref: None,
        actor: "octocat".into(),
        run_id: "1".into(),
        at: crate::store::Timestamp::now(),
    };
    h.orchestrator.record_rejection(&rejection).await.unwrap();
    // Attempts on stacks hoster does not know are not kept.
    h.orchestrator
        .record_rejection(&WebhookRejection {
            stack: "ghost".into(),
            ..rejection.clone()
        })
        .await
        .unwrap();
    let status = h.orchestrator.status().await.unwrap();
    assert_eq!(status[0].rejections, [rejection]);
    assert!(h.store.rejections("ghost", 10).await.unwrap().is_empty());
}

/// A stack whose services declare exec checks: `db` without routes, `app` routed (so its exec
/// check also connects to container port 8080).
pub(super) const SHOP: &str = r#"
version = 1
name = "shop"

[services.db]
image = "docker.io/library/postgres:17"
secrets = { POSTGRES_PASSWORD = "shop/db-password" }

[services.db.health]
exec = ["pg_isready", "-U", "postgres"]
interval = "10ms"
timeout = "20ms"

[services.app]
image = "docker.io/library/shop:1"

[[services.app.routes]]
host = "shop.example.com"
port = 8080

[services.app.health]
exec = ["/bin/check"]
interval = "10ms"
"#;

#[tokio::test]
async fn deploys_run_exec_checks_and_name_them() {
    let h = harness().await;
    h.set_secret("shop/db-password", "hunter22").await;
    h.runtime.script_exec(
        "-db-",
        [
            Ok(ExecOutcome::finished(
                2,
                "no response; password hunter22 rejected\n".into(),
            )),
            Ok(ExecOutcome::finished(0, "accepting connections\n".into())),
        ],
    );
    let applied = h.apply(SHOP).await;
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, DeploymentState::Live, "{deployment:?}");

    // Both execs ran in their containers; app's TCP part went through the prober.
    let execs = h.runtime.execs();
    assert_eq!(
        execs
            .iter()
            .filter(|(c, _)| c == "hoster-shop-db-g1")
            .count(),
        2
    );
    assert!(execs.contains(&(
        "hoster-shop-app-g1".to_owned(),
        vec!["/bin/check".to_owned()]
    )));
    assert!(
        h.prober
            .take_probes()
            .iter()
            .any(|p| p.starts_with("TCP connect to container port 8080 ")),
    );

    let notes: Vec<_> = h
        .notes(deployment.id)
        .await
        .into_iter()
        .map(|(_, message)| message)
        .collect();
    let has = |text: &str| notes.iter().any(|n| n.contains(text));
    assert!(
        has("checking db with exec: pg_isready -U postgres, declared in health.exec"),
        "{notes:#?}"
    );
    assert!(
        has("checking app with exec: /bin/check and TCP connect to container port 8080"),
        "{notes:#?}"
    );
    assert!(
        has("health probe exec: pg_isready -U postgres: exit 2, output \
             \"no response; password <redacted> rejected\""),
        "{notes:#?}"
    );
    assert!(!notes.iter().any(|n| n.contains("hunter22")), "{notes:#?}");
}

#[tokio::test]
async fn an_exec_check_that_cannot_run_fails_the_deploy_at_once() {
    let h = harness().await;
    h.set_secret("shop/db-password", "hunter22").await;
    h.runtime.script_exec(
        "-db-",
        [Ok(ExecOutcome::finished(
            127,
            "Error: runc: exec failed: unable to start container process: exec: \
             \"pg_isready\": executable file not found in $PATH: OCI runtime attempted to \
             invoke a command that was not found\n"
                .into(),
        ))],
    );
    let applied = h.apply(SHOP).await;
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, DeploymentState::Failed);
    assert!(
        deployment
            .error
            .as_deref()
            .unwrap()
            .contains("`health.exec` cannot run: `pg_isready`: executable file not found in $PATH"),
        "{deployment:?}"
    );
    assert_eq!(
        h.runtime
            .execs()
            .iter()
            .filter(|(c, _)| c == "hoster-shop-db-g1")
            .count(),
        1,
        "not retried"
    );
}

/// `app` (start-first, routed) depends on `db` (stop-first, with an exec check), both
/// third-party images, so one apply deploys both (ADR-0056).
pub(super) const DEPS: &str = r#"
version = 1
name = "notes"

[services.app]
image = "docker.io/acme/app:1"
depends_on = ["db"]

[[services.app.routes]]
host = "notes.example.com"
port = 8080

[services.db]
image = "docker.io/library/postgres:17"
update = "stop-first"

[services.db.health]
exec = ["pg_isready"]
interval = "10ms"
"#;

/// The position of the first note of `notes` containing `text`.
fn note_at(notes: &[(DeploymentState, String)], text: &str) -> usize {
    notes
        .iter()
        .position(|(_, n)| n.contains(text))
        .unwrap_or_else(|| panic!("no note {text:?} in {notes:#?}"))
}

/// A stop-first service without a previous generation opens no gap, so it starts in the start
/// phase, and its dependent starts only once it has passed (ADR-0056 Decision 2.1).
#[tokio::test]
async fn a_first_deploy_starts_a_stop_first_dependency_before_its_dependent() {
    let h = harness().await;
    h.runtime.script_exec(
        "-db-",
        [
            Ok(ExecOutcome::finished(2, "no response\n".into())),
            Ok(ExecOutcome::finished(0, "accepting connections\n".into())),
        ],
    );
    let applied = h.apply(DEPS).await;
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    assert_eq!(
        h.runtime.take_starts(),
        ["hoster-notes-db-g1", "hoster-notes-app-g1"]
    );
    let notes = h.notes(deployment.id).await;
    let starting = note_at(&notes, "starting db");
    let passed = note_at(&notes, "health check exec: pg_isready passed");
    let next = note_at(&notes, "db healthy after");
    let checking = note_at(&notes, "checking app with");
    assert!(
        starting < passed && passed < next && next < checking,
        "{notes:#?}"
    );
    assert_eq!(notes[starting], (S::Starting, "starting db".to_owned()));
    let (state, message) = &notes[next];
    assert_eq!(*state, S::HealthChecking);
    assert!(message.ends_with("; starting app"), "{message}");
    // Nothing waits for `Replacing`.
    assert!(!notes.iter().any(|(s, _)| *s == S::Replacing), "{notes:#?}");
}

/// a → b, c → d: d first, then b and c together, then a once both passed. Each service starts
/// as soon as its own dependencies have passed (ADR-0056 Decision 2.1).
#[tokio::test]
async fn a_diamond_starts_each_service_once_its_dependencies_passed() {
    const DIAMOND: &str = r#"
version = 1
name = "notes"

[services.a]
image = "docker.io/library/busybox:1"
depends_on = ["b", "c"]

[services.b]
image = "docker.io/library/busybox:1"
depends_on = ["d"]

[services.c]
image = "docker.io/library/busybox:1"
depends_on = ["d"]

[services.d]
image = "docker.io/library/busybox:1"
"#;
    let h = harness().await;
    let applied = h.apply(DIAMOND).await;
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    assert_eq!(
        h.runtime.take_starts(),
        [
            "hoster-notes-d-g1",
            "hoster-notes-b-g1",
            "hoster-notes-c-g1",
            "hoster-notes-a-g1"
        ]
    );
    let notes = h.notes(deployment.id).await;
    let waves: Vec<&str> = notes
        .iter()
        .map(|(_, n)| n.as_str())
        .filter(|n| n.starts_with("starting ") || n.contains(" healthy after "))
        .collect();
    assert_eq!(waves.len(), 5, "{waves:#?}");
    assert_eq!(waves[0], "starting d");
    assert!(waves[1].starts_with("d healthy after ") && waves[1].ends_with("; starting b, c"));
    // b and c are probed in parallel; the later one to pass starts a.
    let (first, second) = (waves[2], waves[3]);
    assert!(!first.contains("starting"), "{waves:#?}");
    assert!(second.ends_with("; starting a"), "{waves:#?}");
    assert_ne!(first[..1], second[..1]);
    assert!(waves[4].starts_with("a healthy after "), "{waves:#?}");
}

/// A dependency that has never run and is not deployed is ignored, and the log says so
/// (ADR-0056 Decision 2.5).
#[tokio::test]
async fn an_edge_to_a_service_that_never_ran_is_ignored_and_logged() {
    let h = harness().await;
    let stack = NOTES.replace(
        "[services.web]\nimage = \"ghcr.io/acme/notes-web\"",
        "[services.web]\nimage = \"docker.io/acme/notes-web:1\"\ndepends_on = [\"api\"]",
    );
    let applied = h.apply(&stack).await;
    assert_eq!(applied.changed, ["storage", "web"]);
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    let notes = h.notes(deployment.id).await;
    assert!(
        notes.contains(&(
            S::Starting,
            "api has never run; starting web without it".to_owned()
        )),
        "{notes:#?}"
    );
}

/// An unchanged dependency must run when the deployment begins (ADR-0056 Decision 2.4); its
/// monitor status is logged, not awaited.
#[tokio::test]
async fn an_unchanged_dependency_that_is_down_fails_the_deploy() {
    let h = harness().await;
    let first = h
        .finish(h.apply(DEPS).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(first.state, S::Live, "{first:?}");
    let db = h.container("hoster-notes-db-g1").id;
    h.runtime.set_state(&db, ContainerState::Exited(Some(1)));

    let app_changed = DEPS.replace(
        "depends_on = [\"db\"]",
        "depends_on = [\"db\"]\nenv = { A = \"1\" }",
    );
    let failed = h
        .finish(h.apply(&app_changed).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    assert_eq!(
        failed.error.as_deref(),
        Some("db (unchanged) is not running; never started: app")
    );
    assert_eq!(
        h.container_names(),
        ["hoster-notes-app-g1", "hoster-notes-db-g1"]
    );
    let events = h
        .store
        .event_page(&crate::store::EventQuery {
            kinds: [crate::store::EventKind::DeployFailed].into(),
            limit: crate::store::EventQuery::MAX,
            ..crate::store::EventQuery::default()
        })
        .await
        .unwrap()
        .events;
    assert!(
        events[0].message.ends_with("never started: app"),
        "{events:?}"
    );

    // Running again, it is logged with its monitor status and not waited for.
    h.runtime.set_state(&db, ContainerState::Running);
    let app_changed = app_changed.replace("A = \"1\"", "A = \"2\"");
    let live = h
        .finish(h.apply(&app_changed).await.deployment.as_ref().unwrap())
        .await;
    assert_eq!(live.state, S::Live, "{live:?}");
    assert!(
        h.notes(live.id).await.contains(&(
            S::Starting,
            "db (unchanged) is running; its monitor status is unknown".to_owned()
        )),
        "{:#?}",
        h.notes(live.id).await
    );
}

/// A stack whose name order is the wrong start order deploys cleanly on a real rootless
/// podman (ADR-0056): `app` exits at once unless `db`, which needs 2 s to come up, answers
/// (its settle check catches that). Neither publishes a port. Run with
/// `TEST_HOSTER_PODMAN=1 cargo test`.
#[tokio::test]
async fn dependencies_start_in_order_on_real_podman() {
    use crate::registry::Anonymous;
    use crate::runtime::{PodmanRuntime, PodmanSocket};

    if !std::env::var("TEST_HOSTER_PODMAN").is_ok_and(|v| v == "1") {
        eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to deploy on rootless podman");
        return;
    }
    const STACK: &str = r#"
version = 1
name = "hosterdepskbh"

[services.app]
image = "docker.io/library/busybox:1.37"
command = ["sh", "-c", "wget -q -O /dev/null http://db:8080/ || exit 1; mkdir -p /www && exec httpd -f -p 8080 -h /www"]
depends_on = ["db"]
stop_timeout = "1s"

[services.db]
image = "docker.io/library/busybox:1.37"
command = ["sh", "-c", "sleep 2 && mkdir -p /www && echo ok > /www/index.html && exec httpd -f -p 8080 -h /www"]
update = "stop-first"
stop_timeout = "1s"

[services.db.health]
exec = ["wget", "-q", "-O", "/dev/null", "http://127.0.0.1:8080/"]
interval = "500ms"
start_period = "20s"
"#;
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(std::path::PathBuf::from);
    let runtime = Arc::new(
        PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
            .await
            .expect("connect to rootless podman"),
    );
    let orchestrator = Orchestrator::new(
        store.clone(),
        Arc::clone(&runtime),
        Arc::new(RegistryClient::new(Arc::new(Anonymous)).unwrap()),
        PortAllocator::new(store.clone(), PortRange::try_from(28140..=28155).unwrap()),
        SecretStore::new(store.clone(), &SecretKey::generate()),
        Settings::default(),
        Arc::default(),
    );
    let applied = orchestrator.apply(STACK).await.unwrap();
    let first = orchestrator
        .wait(applied.deployment.unwrap().deployment)
        .await
        .unwrap();
    let notes: Vec<String> = store
        .events(first.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| e.message)
        .collect();
    let removed = orchestrator
        .remove(RemoveStack::keeping_volumes("hosterdepskbh"))
        .await;

    assert_eq!(first.state, S::Live, "{first:?} {notes:#?}");
    let at = |text: &str| {
        notes
            .iter()
            .position(|n| n.contains(text))
            .unwrap_or_else(|| panic!("no note {text:?} in {notes:#?}"))
    };
    assert!(at("starting db") < at("db healthy after"), "{notes:#?}");
    assert!(
        notes[at("db healthy after")].ends_with("; starting app"),
        "{notes:#?}"
    );
    assert_eq!(removed.unwrap().containers, 2);
}

/// One inline config file on `api`; `storage` has none.
const WITH_FILES: &str = r#"
version = 1
name = "notes"

[services.api]
image = "docker.io/library/nginx:1.27"
files = { "/etc/nginx/conf.d/default.conf" = { content = "server { listen 8080; }" } }

[[services.api.routes]]
host = "notes.example.com"
port = 8080
"#;

/// ADR-0057 §9: a stack with `files` needs podman 5.4, at apply and again when it deploys.
#[tokio::test]
async fn config_files_need_podman_5_4() {
    let h = harness().await;
    let old = "config files (`files`) need podman 5.4 or later; this host runs podman 4.9.3";

    h.runtime.set_version("4.9.3");
    for result in [
        h.orchestrator.preview(WITH_FILES).await.map(|_| ()),
        h.orchestrator.apply(WITH_FILES).await.map(|_| ()),
    ] {
        let err = result.unwrap_err();
        assert!(matches!(err, OrchestratorError::Unsupported(_)), "{err:?}");
        assert_eq!(err.to_string(), old);
    }
    assert!(h.orchestrator.status().await.unwrap().is_empty());
    // A stack without `files` is none of its business.
    h.set_secret("notes/database-url", "postgres://x").await;
    h.apply(WITH_SECRETS).await;
    let deployed = h.deploy(&[("api", 1), ("multi", 1), ("worker", 1)]).await;
    assert_eq!(h.finish(&deployed).await.state, S::Live);

    // A version hoster cannot read is refused as well, quoting it.
    h.runtime.set_version("unknown");
    let err = h.orchestrator.apply(WITH_FILES).await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "config files (`files`) need podman 5.4 or later; hoster cannot read this host's \
         podman version `unknown`"
    );
}

/// The podman under a stored revision can change (a downgrade, a restored backup): the
/// deployment is rejected in `Validating`, before anything is pulled or created.
#[tokio::test]
async fn a_deployment_with_config_files_is_rejected_on_an_older_podman() {
    let h = harness().await;
    h.runtime.set_version("5.4.0");
    let applied = h.apply(WITH_FILES).await;
    let live = h.finish(&applied.deployment.clone().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");

    h.runtime.set_version("4.9.3");
    let changed = WITH_FILES.replace("listen 8080;", "listen 8080; gzip on;");
    // Apply refuses it outright; a rollback or redeploy of the stored revision reaches
    // `Validating`.
    assert!(h.orchestrator.apply(&changed).await.is_err());
    h.runtime.set_version("5.4.0");
    let second = h.apply(&changed).await;
    let second = h.finish(&second.deployment.clone().unwrap()).await;
    assert_eq!(second.state, S::Live, "{second:?}");

    h.runtime.set_version("4.9.3");
    let created = h.runtime.calls().len();
    let back = h.orchestrator.rollback("notes", live.id).await.unwrap();
    let rejected = h.finish(&back).await;
    assert_eq!(rejected.state, S::Rejected, "{rejected:?}");
    assert_eq!(
        rejected.error.as_deref(),
        Some("config files (`files`) need podman 5.4 or later; this host runs podman 4.9.3")
    );
    assert_eq!(h.runtime.calls().len(), created, "nothing reached podman");
}

/// A monitoring stack as ADR-0057 has it in mind: an inline config, a secret file with the
/// defaults and one with a mode and an owner. `exporter` has no files.
const MONITORING: &str = r#"
version = 1
name = "monitoring"

[services.prometheus]
image = "docker.io/prom/prometheus:v3.0.0"
user = "65534"
read_only = true

[services.prometheus.files]
"/etc/prometheus/prometheus.yml" = { content = "scrape_interval: 15s\n" }
"/run/secrets/remote-write.token" = { secret = "monitoring/token" }
"/etc/prometheus/web.yml" = { secret = "monitoring/web", mode = "0440", owner = "0:65534" }

[services.prometheus.health]
exec = ["promtool", "check", "healthy"]
interval = "10ms"

[services.exporter]
image = "docker.io/prom/node-exporter:v1.8.0"
"#;

/// ADR-0057 §3, §4 and §6: the engine resolves content, mode and owner and hands the files
/// to the runtime with the container; nothing that prints the spec shows a file's content.
#[tokio::test]
async fn config_files_go_to_the_runtime_with_their_container() {
    let h = harness().await;
    h.set_secret("monitoring/token", "t0ken-value").await;
    h.set_secret("monitoring/web", "basic_auth_users:\n  admin: $2y$hash\n")
        .await;
    let applied = h.apply(MONITORING).await;
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");

    let spec = h.spec_of("hoster-monitoring-prometheus-g1");
    let files: Vec<_> = spec
        .files
        .iter()
        .map(|f| {
            (
                f.path.to_string(),
                f.mode.to_string(),
                f.owner.to_string(),
                f.content.expose().to_owned(),
            )
        })
        .collect();
    let row = |path: &str, mode: &str, owner: &str, content: &str| {
        (
            path.to_owned(),
            mode.to_owned(),
            owner.to_owned(),
            content.to_owned(),
        )
    };
    assert_eq!(
        files,
        [
            row(
                "/etc/prometheus/prometheus.yml",
                "0444",
                "0:0",
                "scrape_interval: 15s\n"
            ),
            row(
                "/etc/prometheus/web.yml",
                "0440",
                "0:65534",
                "basic_auth_users:\n  admin: $2y$hash\n"
            ),
            // A secret file defaults to the container's user, whoever podman says that is.
            row(
                "/run/secrets/remote-write.token",
                "0400",
                "user",
                "t0ken-value"
            ),
        ]
    );
    // File secrets are not environment variables.
    assert!(spec.secrets.is_empty() && !spec.env.contains_key("token"));
    let debug = format!("{spec:?}");
    for content in ["t0ken-value", "$2y$hash", "scrape_interval"] {
        assert!(!debug.contains(content), "{content} in {debug}");
    }
    assert!(h.spec_of("hoster-monitoring-exporter-g1").files.is_empty());

    // Nothing of a file's content reaches the deployment's events either.
    let events = h.store.events(deployment.id).await.unwrap();
    assert!(
        events
            .iter()
            .all(|e| !e.message.as_deref().unwrap_or("").contains("t0ken"))
    );
}

/// ADR-0057 §3: only the pulled image knows its volumes, so `Pulling` is where a file in one
/// fails the deploy, before any container exists.
#[tokio::test]
async fn a_config_file_in_an_image_volume_fails_the_deploy_in_pulling() {
    let h = harness().await;
    h.set_secret("monitoring/token", "t0ken-value").await;
    h.set_secret("monitoring/web", "x").await;
    h.runtime
        .set_image_volumes("prom/prometheus", &["/prometheus", "/etc/prometheus/"]);
    let applied = h.apply(MONITORING).await;
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    let error = failed.error.unwrap();
    assert!(
        error.starts_with(
            "service prometheus: `/etc/prometheus/prometheus.yml` is in the volume mounted at \
             `/etc/prometheus`; a volume hides files uploaded there; the image \
             docker.io/prom/prometheus@sha256:"
        ) && error.ends_with("declares that volume"),
        "{error}"
    );
    assert!(
        !h.runtime.calls().contains(&Operation::CreateContainer),
        "{:?}",
        h.runtime.calls()
    );

    // A file above an image volume is as wrong: the start would fail (architecture §1.7).
    let h = harness().await;
    h.set_secret("monitoring/token", "t0ken-value").await;
    h.set_secret("monitoring/web", "x").await;
    h.runtime
        .set_image_volumes("prom/prometheus", &["/run/secrets/remote-write.token/data"]);
    let applied = h.apply(MONITORING).await;
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    assert!(
        failed
            .error
            .as_deref()
            .unwrap()
            .contains("is a parent directory of the volume mounted at"),
        "{failed:?}"
    );
}

/// The runtime removes a container whose files it could not write; the deployment fails with
/// podman's reason and the old generation keeps running.
#[tokio::test]
async fn a_failed_upload_fails_the_deployment() {
    let h = harness().await;
    h.set_secret("monitoring/token", "t0ken-value").await;
    h.set_secret("monitoring/web", "x").await;
    let applied = h.apply(MONITORING).await;
    assert_eq!(
        h.finish(applied.deployment.as_ref().unwrap()).await.state,
        S::Live
    );

    h.runtime.fail_next(
        Operation::UploadFiles,
        RuntimeError::ConfigFiles {
            message: "error setting ownership of \"/etc/prometheus/web.yml\" to 70000:0: \
                      invalid argument"
                .into(),
        },
    );
    let changed = MONITORING.replace("owner = \"0:65534\"", "owner = \"65535:0\"");
    let applied = h.apply(&changed).await;
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    assert_eq!(
        failed.error.as_deref(),
        Some(
            "cannot create hoster-monitoring-prometheus-g2: cannot write its config files: \
             error setting ownership of \"/etc/prometheus/web.yml\" to 70000:0: invalid argument"
        )
    );
    let names: Vec<String> = h.runtime.containers().into_iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        [
            "hoster-monitoring-exporter-g1",
            "hoster-monitoring-prometheus-g1"
        ]
    );
    assert!(
        h.container("hoster-monitoring-prometheus-g1")
            .state
            .is_running()
    );
}

/// A secret can grow after the stack was applied, so its size is checked when it is written
/// (ADR-0057 §6). The message names the secret and the file, never the value.
#[tokio::test]
async fn an_oversized_file_secret_fails_the_deploy() {
    let h = harness().await;
    let big = "s3cret-".repeat(256 * 1024 / 7 + 1);
    h.set_secret("monitoring/token", &big).await;
    h.set_secret("monitoring/web", "x").await;
    let applied = h.apply(MONITORING).await;
    let failed = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(failed.state, S::Failed, "{failed:?}");
    assert_eq!(
        failed.error.as_deref(),
        Some(
            format!(
                "cannot create hoster-monitoring-prometheus-g1: secret `monitoring/token` (for \
                 file /run/secrets/remote-write.token) is {} bytes; a file holds at most 256 KiB",
                big.len()
            )
            .as_str()
        )
    );
    assert!(
        h.runtime
            .containers()
            .iter()
            .all(|c| !c.name.contains("prometheus"))
    );
}

/// ADR-0057 §6: exec output is redacted for file secrets too, each line on its own.
#[tokio::test]
async fn exec_output_is_redacted_for_each_line_of_a_file_secret() {
    let h = harness().await;
    h.set_secret("monitoring/token", "t0ken-value").await;
    h.set_secret(
        "monitoring/web",
        "basic_auth_users:\n  admin: $2y$hash-of-admin\n",
    )
    .await;
    h.runtime.script_exec(
        "-prometheus-",
        [
            Ok(ExecOutcome::finished(
                1,
                "bad line in web.yml: admin: $2y$hash-of-admin (token t0ken-value)\n".into(),
            )),
            Ok(ExecOutcome::finished(0, "healthy\n".into())),
        ],
    );
    let applied = h.apply(MONITORING).await;
    let deployment = h.finish(applied.deployment.as_ref().unwrap()).await;
    assert_eq!(deployment.state, S::Live, "{deployment:?}");
    let notes: Vec<_> = h
        .notes(deployment.id)
        .await
        .into_iter()
        .map(|(_, message)| message)
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("output \"bad line in web.yml: <redacted> (token <redacted>)\"")),
        "{notes:#?}"
    );
    assert!(
        !notes
            .iter()
            .any(|n| n.contains("hash-of-admin") || n.contains("t0ken")),
        "{notes:#?}"
    );
}

impl Harness {
    /// Writes `text` to `path` under the fixture host's root (`proc/meminfo`, …).
    fn host_file(&self, path: &str, text: &str) {
        let path = self._dir.path().join("host").join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// The controllers the fixture host's user manager (uid 1000) delegates.
    pub(super) fn delegate(&self, controllers: &str) {
        self.host_file(
            "sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/cgroup.controllers",
            controllers,
        );
    }
}

/// `api` limits its memory and CPUs; `storage` sets no limit.
const WITH_LIMITS: &str = r#"
version = 1
name = "notes"

[services.api]
image = "docker.io/library/nginx:1.27"
limits = { memory = "64MiB", cpus = 1.5 }

[[services.api.routes]]
host = "notes.example.com"
port = 8080

[services.storage]
image = "docker.io/library/redis:7"
"#;

/// ADR-0059 §5: a limit whose controller the user manager does not delegate rejects the stack
/// at apply, before any tag is resolved, with the one fix; delegated, it reaches podman.
#[tokio::test]
async fn limits_need_their_cgroup_controllers_at_apply() {
    let h = harness().await;
    h.delegate("memory pids\n");
    for result in [
        h.orchestrator.preview(WITH_LIMITS).await.map(|_| ()),
        h.orchestrator.apply(WITH_LIMITS).await.map(|_| ()),
    ] {
        let err = result.unwrap_err();
        assert!(matches!(err, OrchestratorError::Undelegated(_)), "{err:?}");
        let message = err.to_string();
        assert!(
            message.starts_with(
                "hoster's user manager does not delegate the cgroup controller cpu, so \
                 `limits.cpus` of api cannot be enforced without it; to fix it, create \
                 /etc/systemd/system/user@1000.service.d/hoster-delegate.conf containing \
                 `[Service]` and `Delegate=cpu memory pids`"
            ),
            "{message}"
        );
    }
    assert!(h.orchestrator.status().await.unwrap().is_empty());
    assert_eq!(
        h.resolver.calls.load(Ordering::SeqCst),
        0,
        "no tag resolved"
    );

    h.delegate("cpuset cpu io memory pids\n");
    let applied = h.apply(WITH_LIMITS).await;
    let live = h.finish(&applied.deployment.clone().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");
    let api = h.spec_of("hoster-notes-api-g1").limits;
    assert_eq!(api.memory.map(u64::from), Some(64 << 20));
    assert_eq!(api.cpus.map(i64::from), Some(1_500_000_000));
    assert_eq!(api.pids, None);
    assert!(h.spec_of("hoster-notes-storage-g1").limits.is_empty());
}

/// A controller can go under a stored revision (a changed drop-in, another host): a rollback
/// or redeploy is rejected in `Validating`, with apply's text, before anything reaches podman.
#[tokio::test]
async fn a_deployment_with_limits_is_rejected_in_validating_without_the_controller() {
    let h = harness().await;
    h.delegate("cpu memory pids\n");
    let applied = h.apply(WITH_LIMITS).await;
    let first = h.finish(&applied.deployment.clone().unwrap()).await;
    assert_eq!(first.state, S::Live, "{first:?}");
    let second = h
        .apply(&WITH_LIMITS.replace("cpus = 1.5", "cpus = 1"))
        .await;
    let second = h.finish(&second.deployment.clone().unwrap()).await;
    assert_eq!(second.state, S::Live, "{second:?}");

    h.delegate("memory pids\n");
    let expected = h
        .orchestrator
        .apply(WITH_LIMITS)
        .await
        .unwrap_err()
        .to_string();
    let created = h.runtime.calls().len();
    let back = h.orchestrator.rollback("notes", first.id).await.unwrap();
    let rejected = h.finish(&back).await;
    assert_eq!(rejected.state, S::Rejected, "{rejected:?}");
    assert_eq!(rejected.error.as_deref(), Some(expected.as_str()));
    assert_eq!(h.runtime.calls().len(), created, "nothing reached podman");
}

/// Without a readable `cgroup.controllers` hoster cannot tell whether a limit holds: only
/// stacks that set one are rejected.
#[tokio::test]
async fn an_unreadable_controller_list_rejects_only_stacks_with_limits() {
    let h = harness().await;
    let err = h.orchestrator.apply(WITH_LIMITS).await.unwrap_err();
    assert!(matches!(err, OrchestratorError::Undelegated(_)), "{err:?}");
    let message = err.to_string();
    assert!(
        message.contains("user@1000.service/cgroup.controllers cannot be read"),
        "{message}"
    );
    let unlimited =
        WITH_LIMITS.replace("limits = { memory = \"64MiB\", cpus = 1.5 }", "limits = {}");
    let applied = h.apply(&unlimited).await;
    let live = h.finish(&applied.deployment.clone().unwrap()).await;
    assert_eq!(live.state, S::Live, "{live:?}");
}

/// The editor's live check reports a missing controller at each limit that needs it, and a
/// limit larger than the host as a warning at its key; apply returns the same warnings.
#[tokio::test]
async fn check_file_and_apply_report_limits_against_the_host() {
    let h = harness().await;
    h.delegate("memory pids\n");
    let report = h.orchestrator.check_file(WITH_LIMITS).await.unwrap();
    assert!(report.warnings.is_empty(), "{report:?}");
    let errors: Vec<(String, Option<(usize, usize)>)> = report
        .errors
        .iter()
        .map(|d| (d.path.to_string(), d.location.map(|l| (l.line, l.column))))
        .collect();
    assert_eq!(
        errors,
        [("services.api.limits.cpus".to_owned(), Some((7, 30)))]
    );
    assert!(report.errors[0].message.contains("hoster-delegate.conf"));

    // Unreadable: every limit set is in doubt.
    std::fs::remove_file(h._dir.path().join(
        "host/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/cgroup.controllers",
    ))
    .unwrap();
    let report = h.orchestrator.check_file(WITH_LIMITS).await.unwrap();
    let paths: Vec<String> = report.errors.iter().map(|d| d.path.to_string()).collect();
    assert_eq!(
        paths,
        ["services.api.limits.memory", "services.api.limits.cpus"]
    );

    h.delegate("cpu memory pids\n");
    h.host_file("proc/meminfo", "MemTotal:       32768 kB\n");
    h.host_file("sys/devices/system/cpu/online", "0\n");
    let report = h.orchestrator.check_file(WITH_LIMITS).await.unwrap();
    assert!(report.errors.is_empty(), "{report:?}");
    let warnings: Vec<(String, String)> = report
        .warnings
        .iter()
        .map(|d| (d.path.to_string(), d.message.clone()))
        .collect();
    assert_eq!(
        warnings,
        [
            (
                "services.api.limits.memory".to_owned(),
                "`memory = \"64MiB\"` is more than this host's 32 MiB of memory (MemTotal), so \
                 it caps nothing"
                    .to_owned()
            ),
            (
                "services.api.limits.cpus".to_owned(),
                "`cpus = 1.5` is more than this host's 1 CPU, so it caps nothing".to_owned()
            ),
        ]
    );
    let applied = h.apply(WITH_LIMITS).await;
    assert_eq!(applied.warnings, report.warnings);
}
