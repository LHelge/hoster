use std::collections::BTreeMap;
use std::sync::Mutex;

use tempfile::TempDir;
use wiremock::matchers::{basic_auth, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::registry::RegistryCredential;
use crate::store::{EventKind, ServiceUpdates};

fn digest(n: u8) -> Digest {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
        .parse()
        .unwrap()
}

fn check(stack: &str, service: &str, image: &str, pinned: u8) -> ImageCheck {
    ImageCheck {
        stack: stack.to_owned(),
        service: service.to_owned(),
        image: image.parse().unwrap(),
        digest: digest(pinned),
    }
}

/// What the fake targets hand out, and what the checker told them.
#[derive(Default)]
struct Targets {
    services: Mutex<Vec<ImageCheck>>,
    events: Mutex<Vec<NewEvent>>,
    announced: Mutex<Vec<String>>,
    passes: Mutex<u32>,
    auto_updates: Mutex<u32>,
}

#[async_trait]
impl UpdateTargets for Arc<Targets> {
    async fn checkable(&self) -> anyhow::Result<Vec<ImageCheck>> {
        *self.passes.lock().unwrap() += 1;
        Ok(self.services.lock().unwrap().clone())
    }

    async fn record(&self, event: NewEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn announce(&self, stack: &str) {
        self.announced.lock().unwrap().push(stack.to_owned());
    }

    async fn auto_update(&self) -> anyhow::Result<()> {
        *self.auto_updates.lock().unwrap() += 1;
        Ok(())
    }
}

impl Targets {
    fn set(&self, services: Vec<ImageCheck>) {
        *self.services.lock().unwrap() = services;
    }

    fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut *self.events.lock().unwrap())
            .into_iter()
            .map(|e| {
                assert_eq!(e.kind, EventKind::UpdateAvailable);
                e.message
            })
            .collect()
    }
}

/// How the fake registry answers for one reference.
#[derive(Debug, Clone)]
enum Answer {
    Digest(u8),
    Tags(Vec<&'static str>),
    RateLimited(Option<Duration>),
    NotFound,
}

impl Answer {
    fn error(&self, image: &ImageRef) -> RegistryError {
        match self {
            Self::RateLimited(retry_after) => RegistryError::RateLimited {
                registry: image.registry().to_owned(),
                retry_after: *retry_after,
            },
            _ => RegistryError::NotFound {
                image: image.to_string(),
            },
        }
    }
}

/// Answers by `registry/repository:tag` for digests and `registry/repository` for tag lists,
/// and counts the requests.
#[derive(Default)]
struct Registry {
    answers: Mutex<BTreeMap<String, Answer>>,
    requests: Mutex<Vec<String>>,
}

impl Registry {
    fn answer(&self, key: &str, answer: Answer) {
        self.answers.lock().unwrap().insert(key.to_owned(), answer);
    }

    fn requests(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }

    fn lookup(&self, key: String) -> Option<Answer> {
        self.requests.lock().unwrap().push(key.clone());
        self.answers.lock().unwrap().get(&key).cloned()
    }
}

#[async_trait]
impl UpdateRegistry for Registry {
    async fn resolve(&self, image: &ImageRef) -> Result<PinnedImage, RegistryError> {
        let key = format!(
            "{}/{}:{}",
            image.registry(),
            image.repository(),
            image.tag().unwrap()
        );
        match self.lookup(key) {
            Some(Answer::Digest(n)) => Ok(image.clone().pin(digest(n))),
            Some(other) => Err(other.error(image)),
            None => Err(Answer::NotFound.error(image)),
        }
    }

    async fn tags(&self, image: &ImageRef) -> Result<Vec<String>, RegistryError> {
        let key = format!("{}/{}", image.registry(), image.repository());
        match self.lookup(format!("tags {key}")) {
            Some(Answer::Tags(tags)) => Ok(tags.into_iter().map(str::to_owned).collect()),
            Some(other) => Err(other.error(image)),
            None => Err(Answer::NotFound.error(image)),
        }
    }
}

struct Fixture {
    _dir: TempDir,
    store: Store,
    targets: Arc<Targets>,
    registry: Arc<Registry>,
    checker: UpdateChecker,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let targets = Arc::new(Targets::default());
    let registry = Arc::new(Registry::default());
    let checker = UpdateChecker::new(
        Box::new(Arc::clone(&targets)),
        Arc::clone(&registry) as Arc<dyn UpdateRegistry>,
        store.clone(),
    );
    Fixture {
        _dir: dir,
        store,
        targets,
        registry,
        checker,
    }
}

impl Fixture {
    async fn updates(&self) -> Vec<ServiceUpdates> {
        self.store.image_updates().await.unwrap()
    }

    /// `(service, kind, tag)` of every stored candidate.
    async fn found(&self) -> Vec<(String, UpdateKind, String)> {
        self.updates()
            .await
            .into_iter()
            .flat_map(|u| {
                u.candidates
                    .into_iter()
                    .map(move |c| (u.service.clone(), c.candidate.kind, c.candidate.tag))
            })
            .collect()
    }
}

#[tokio::test]
async fn a_moved_tag_is_a_digest_update_found_once() {
    let mut f = fixture().await;
    f.targets.set(vec![check(
        "db",
        "postgres",
        "docker.io/library/postgres:16",
        1,
    )]);
    f.registry
        .answer("docker.io/library/postgres:16", Answer::Digest(2));
    f.registry.answer(
        "tags docker.io/library/postgres",
        Answer::Tags(vec!["16", "17"]),
    );

    f.checker.check().await.unwrap();
    let updates = f.updates().await;
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].error, None);
    let candidates = &updates[0].candidates;
    assert_eq!(candidates[0].candidate.kind, UpdateKind::Digest);
    assert_eq!(candidates[0].candidate.digest, Some(digest(2)));
    assert_eq!(
        f.found().await,
        [
            ("postgres".into(), UpdateKind::Digest, "16".into()),
            ("postgres".into(), UpdateKind::Major, "17".into()),
        ]
    );
    let first_seen = candidates[0].first_seen;
    assert_eq!(
        f.targets.take_events(),
        [
            "postgres has a new image for docker.io/library/postgres:16",
            "postgres has a major update: docker.io/library/postgres:17",
        ]
    );
    assert_eq!(*f.targets.announced.lock().unwrap(), ["db"]);
    // Each pass ends by applying what the policies allow.
    assert_eq!(*f.targets.auto_updates.lock().unwrap(), 1);

    // Found again: no new event, and the first-seen time stands.
    f.checker.check().await.unwrap();
    assert!(f.targets.take_events().is_empty());
    assert_eq!(f.updates().await[0].candidates[0].first_seen, first_seen);

    // The tag moves on again: a new candidate, a new event.
    f.registry
        .answer("docker.io/library/postgres:16", Answer::Digest(3));
    f.checker.check().await.unwrap();
    assert_eq!(f.targets.take_events().len(), 1);
    assert_eq!(
        f.updates().await[0].candidates[0].candidate.digest,
        Some(digest(3))
    );

    // The revision moves to what the tag serves: the digest update is gone.
    f.targets.set(vec![check(
        "db",
        "postgres",
        "docker.io/library/postgres:16",
        3,
    )]);
    f.checker.check().await.unwrap();
    assert_eq!(
        f.found().await,
        [("postgres".into(), UpdateKind::Major, "17".into())]
    );
}

#[tokio::test]
async fn newer_versions_are_found_per_kind() {
    let mut f = fixture().await;
    f.targets.set(vec![check(
        "photos",
        "server",
        "ghcr.io/immich-app/immich-server:v2.3.1",
        1,
    )]);
    f.registry
        .answer("ghcr.io/immich-app/immich-server:v2.3.1", Answer::Digest(1));
    f.registry.answer(
        "tags ghcr.io/immich-app/immich-server",
        Answer::Tags(vec![
            "v2.3.1",
            "v2.3.4",
            "v2.4.0",
            "v2.4.1",
            "v3.0.0",
            "v3.1.0-rc.1",
            "2.9.9",
            "pr-1234",
            "main",
        ]),
    );
    f.checker.check().await.unwrap();
    assert_eq!(
        f.found().await,
        [
            ("server".into(), UpdateKind::Patch, "v2.3.4".into()),
            ("server".into(), UpdateKind::Minor, "v2.4.1".into()),
            ("server".into(), UpdateKind::Major, "v3.0.0".into()),
        ]
    );
    assert!(
        f.updates().await[0]
            .candidates
            .iter()
            .all(|c| c.candidate.digest.is_none())
    );
    assert_eq!(
        f.targets.take_events()[1],
        "server has a minor update: ghcr.io/immich-app/immich-server:v2.4.1"
    );

    // Only a patch is out: the others go.
    f.registry.answer(
        "tags ghcr.io/immich-app/immich-server",
        Answer::Tags(vec!["v2.3.1", "v2.3.2"]),
    );
    f.checker.check().await.unwrap();
    assert_eq!(
        f.found().await,
        [("server".into(), UpdateKind::Patch, "v2.3.2".into())]
    );
}

#[tokio::test]
async fn tags_that_are_not_versions_get_only_the_digest_check() {
    let mut f = fixture().await;
    f.targets.set(vec![check(
        "web",
        "nginx",
        "docker.io/library/nginx:latest",
        1,
    )]);
    f.registry
        .answer("docker.io/library/nginx:latest", Answer::Digest(1));
    f.checker.check().await.unwrap();
    assert_eq!(f.registry.requests(), ["docker.io/library/nginx:latest"]);
    assert!(f.found().await.is_empty());
    assert_eq!(f.updates().await[0].error, None);
}

#[tokio::test]
async fn each_tag_and_repository_is_asked_once_per_pass() {
    let mut f = fixture().await;
    f.targets.set(vec![
        check("a", "db", "docker.io/library/postgres:16.4", 1),
        check("b", "db", "docker.io/library/postgres:16.4", 1),
        check("c", "db", "docker.io/library/postgres:16.2", 1),
    ]);
    f.registry
        .answer("docker.io/library/postgres:16.4", Answer::Digest(1));
    f.registry
        .answer("docker.io/library/postgres:16.2", Answer::Digest(1));
    f.registry.answer(
        "tags docker.io/library/postgres",
        Answer::Tags(vec!["16.2", "16.4", "16.6"]),
    );
    f.checker.check().await.unwrap();
    assert_eq!(
        f.registry.requests(),
        [
            "docker.io/library/postgres:16.4",
            "tags docker.io/library/postgres",
            "docker.io/library/postgres:16.2",
        ]
    );
    let found = f.found().await;
    assert_eq!(found.len(), 3, "{found:?}");
    assert!(
        found
            .iter()
            .all(|(_, kind, tag)| *kind == UpdateKind::Minor && tag == "16.6")
    );
}

#[tokio::test]
async fn a_rate_limited_registry_is_left_alone_and_earlier_results_kept() {
    let mut f = fixture().await;
    f.targets.set(vec![
        check("a", "db", "docker.io/library/postgres:16", 1),
        check("a", "cache", "docker.io/library/valkey:8", 1),
        check("a", "app", "ghcr.io/acme/tool:latest", 1),
    ]);
    f.registry
        .answer("docker.io/library/postgres:16", Answer::Digest(2));
    f.registry
        .answer("tags docker.io/library/postgres", Answer::Tags(vec!["16"]));
    f.registry
        .answer("docker.io/library/valkey:8", Answer::Digest(1));
    f.registry
        .answer("tags docker.io/library/valkey", Answer::Tags(vec!["8"]));
    f.registry
        .answer("ghcr.io/acme/tool:latest", Answer::Digest(5));
    f.checker.check().await.unwrap();
    assert_eq!(f.found().await.len(), 2);
    f.registry.requests();

    // Docker Hub rate-limits the first request and asks for an hour.
    f.registry.answer(
        "docker.io/library/postgres:16",
        Answer::RateLimited(Some(Duration::from_secs(3600))),
    );
    f.checker.check().await.unwrap();
    // valkey was not asked; ghcr was.
    assert_eq!(
        f.registry.requests(),
        ["docker.io/library/postgres:16", "ghcr.io/acme/tool:latest"]
    );
    let updates = f.updates().await;
    let by_service = |name: &str| updates.iter().find(|u| u.service == name).unwrap();
    let db = by_service("db");
    assert!(
        db.error
            .as_deref()
            .is_some_and(|e| e.contains("rate-limiting") && e.contains("retry after 1h")),
        "{db:?}"
    );
    // Its earlier candidate stays.
    assert_eq!(db.candidates.len(), 1);
    assert!(
        by_service("cache")
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("not checked: ")),
    );
    assert_eq!(by_service("app").error, None);

    // The next pass within the hour still leaves Docker Hub alone.
    f.checker.check().await.unwrap();
    assert_eq!(f.registry.requests(), ["ghcr.io/acme/tool:latest"]);

    // After it, Docker Hub is asked again, and the errors clear.
    for (until, _) in f.checker.backoff.values_mut() {
        *until = Instant::now();
    }
    f.registry
        .answer("docker.io/library/postgres:16", Answer::Digest(2));
    f.checker.check().await.unwrap();
    assert_eq!(f.registry.requests().len(), 5);
    assert!(f.updates().await.iter().all(|u| u.error.is_none()));
}

#[tokio::test]
async fn an_error_for_one_image_is_recorded_without_backing_off() {
    let mut f = fixture().await;
    f.targets.set(vec![
        check("a", "gone", "docker.io/acme/gone:1", 1),
        check("a", "db", "docker.io/library/postgres:latest", 1),
    ]);
    f.registry
        .answer("docker.io/library/postgres:latest", Answer::Digest(1));
    f.checker.check().await.unwrap();
    assert_eq!(f.registry.requests().len(), 2);
    let gone = f
        .updates()
        .await
        .into_iter()
        .find(|u| u.service == "gone")
        .unwrap();
    assert!(gone.error.unwrap().contains("not found"));
}

#[tokio::test]
async fn results_follow_the_revision_and_go_with_the_service_or_stack() {
    let mut f = fixture().await;
    f.store.add_revision("photos", "x").await.unwrap();
    f.targets.set(vec![
        check(
            "photos",
            "server",
            "ghcr.io/immich-app/immich-server:v2.3.1",
            1,
        ),
        check("photos", "db", "docker.io/library/postgres:16", 1),
    ]);
    f.registry
        .answer("ghcr.io/immich-app/immich-server:v2.3.1", Answer::Digest(1));
    f.registry
        .answer("ghcr.io/immich-app/immich-server:v2.4.0", Answer::Digest(1));
    f.registry.answer(
        "tags ghcr.io/immich-app/immich-server",
        Answer::Tags(vec!["v2.3.1", "v2.4.0"]),
    );
    f.registry
        .answer("docker.io/library/postgres:16", Answer::Digest(2));
    f.registry
        .answer("tags docker.io/library/postgres", Answer::Tags(vec!["16"]));
    f.checker.check().await.unwrap();
    assert_eq!(f.found().await.len(), 2);

    // The update was applied: the candidate for the old tag goes even before the next pass
    // finds anything, and only db is left with one.
    f.targets.set(vec![
        check(
            "photos",
            "server",
            "ghcr.io/immich-app/immich-server:v2.4.0",
            1,
        ),
        check("photos", "db", "docker.io/library/postgres:16", 1),
    ]);
    f.registry.answer(
        "tags ghcr.io/immich-app/immich-server",
        Answer::RateLimited(None),
    );
    f.checker.check().await.unwrap();
    let server = f
        .updates()
        .await
        .into_iter()
        .find(|u| u.service == "server")
        .unwrap();
    assert_eq!(server.tag, "v2.4.0");
    assert!(server.error.is_some());
    assert!(server.candidates.is_empty());

    // db is no longer checkable (pinned by digest only): its row goes.
    f.targets.set(vec![check(
        "photos",
        "server",
        "ghcr.io/immich-app/immich-server:v2.4.0",
        1,
    )]);
    f.checker.check().await.unwrap();
    let services: Vec<String> = f.updates().await.into_iter().map(|u| u.service).collect();
    assert_eq!(services, ["server"]);

    // Removing the stack removes the rest at once.
    assert!(f.store.remove_stack("photos").await.unwrap());
    assert!(f.updates().await.is_empty());
}

#[tokio::test]
async fn the_registry_client_checks_with_the_stored_credential() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    let challenge = ResponseTemplate::new(401).insert_header(
        "www-authenticate",
        format!(r#"Bearer realm="{realm}",service="registry.test""#).as_str(),
    );
    Mock::given(path("/v2/acme/app/manifests/1.2"))
        .respond_with(challenge.clone())
        .with_priority(10)
        .mount(&server)
        .await;
    Mock::given(path("/v2/acme/app/tags/list"))
        .respond_with(challenge)
        .with_priority(10)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(basic_auth("bot", "s3cret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"token": "t"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/acme/app/manifests/1.2"))
        .and(wiremock::matchers::header("authorization", "Bearer t"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("docker-content-digest", digest(1).as_ref()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/acme/app/tags/list"))
        .and(wiremock::matchers::header("authorization", "Bearer t"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"tags": ["1.2", "1.3", "2.0"]})),
        )
        .mount(&server)
        .await;
    let credentials = BTreeMap::from([(
        "registry.test".to_owned(),
        RegistryCredential::new("bot", "s3cret"),
    )]);
    let client = RegistryClient::new(Arc::new(credentials))
        .unwrap()
        .with_endpoint("registry.test", server.uri().parse().unwrap());

    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("state")).await.unwrap();
    let targets = Arc::new(Targets::default());
    targets.set(vec![check("a", "app", "registry.test/acme/app:1.2", 1)]);
    let mut checker = UpdateChecker::new(
        Box::new(Arc::clone(&targets)),
        Arc::new(client),
        store.clone(),
    );
    checker.check().await.unwrap();
    let updates = store.image_updates().await.unwrap();
    let tags: Vec<&str> = updates[0]
        .candidates
        .iter()
        .map(|c| c.candidate.tag.as_str())
        .collect();
    assert_eq!(tags, ["1.3", "2.0"]);
}

#[test]
fn the_next_pass_is_due_an_interval_after_the_last() {
    let hour = Duration::from_secs(3600);
    let six = 6 * hour;
    let ms = |d: Duration| i64::try_from(d.as_millis()).unwrap();
    // Never ran: soon after start.
    assert_eq!(
        UpdateChecker::delay(None, 0, six, 0.0),
        UpdateChecker::FIRST_PASS_DELAY
    );
    // Ran two hours ago: four to go, give or take the jitter.
    assert_eq!(
        UpdateChecker::delay(Some(0), ms(2 * hour), six, 0.0),
        4 * hour
    );
    assert_eq!(
        UpdateChecker::delay(Some(0), ms(2 * hour), six, 0.1),
        Duration::from_secs(4 * 3600 + 2160)
    );
    // Jitter beyond ±10 % is clamped.
    assert_eq!(
        UpdateChecker::delay(Some(0), 0, six, -5.0),
        six.mul_f64(0.9)
    );
    // Overdue (hoster was down), or the clock went back: due now, or at most an interval.
    assert_eq!(
        UpdateChecker::delay(Some(0), ms(10 * hour), six, 0.0),
        Duration::ZERO
    );
    assert_eq!(UpdateChecker::delay(Some(ms(hour)), 0, six, 0.0), six);
    for _ in 0..100 {
        let jitter = UpdateChecker::random_jitter();
        assert!((-JITTER..=JITTER).contains(&jitter), "{jitter}");
    }
}

#[tokio::test]
async fn check_now_runs_a_pass_and_the_schedule_resumes() {
    let f = fixture().await;
    // A pass ran just now: the next is hours away.
    f.store
        .kv_set(LAST_PASS_KEY, &Timestamp::now().as_millis().to_string())
        .await
        .unwrap();
    let handle = f.checker.handle();
    let token = CancellationToken::new();
    let task = tokio::spawn(f.checker.run(token.clone()));
    let passes = || *f.targets.passes.lock().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(passes(), 0);

    handle.check_now();
    for _ in 0..100 {
        if passes() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(passes(), 1);

    // A reschedule alone runs nothing.
    handle.reschedule();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(passes(), 1);

    token.cancel();
    task.await.unwrap();
}

#[tokio::test]
async fn settings_round_trip_with_bounds() {
    let f = fixture().await;
    assert_eq!(
        f.store.update_settings().await.unwrap().interval,
        crate::store::CheckInterval::DEFAULT
    );
    let interval = crate::store::CheckInterval::try_from(Duration::from_secs(3600)).unwrap();
    f.store
        .set_update_settings(&crate::store::UpdateSettings { interval })
        .await
        .unwrap();
    assert_eq!(f.store.update_settings().await.unwrap().interval, interval);
    assert_eq!(interval.to_string(), "1h");
    for out in [
        Duration::from_secs(3599),
        Duration::from_secs(7 * 86400 + 1),
    ] {
        let err = crate::store::CheckInterval::try_from(out).unwrap_err();
        assert!(err.contains("between 1h and 7days"), "{err}");
    }
}
