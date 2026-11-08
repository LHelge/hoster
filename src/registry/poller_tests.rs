//! The poller against a fake orchestrator, a fake registry and an in-memory observation store,
//! with paused time.

use std::sync::Mutex;

use super::*;

const INTERVAL: Duration = Duration::from_secs(300);

fn digest(n: u8) -> Digest {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
        .parse()
        .unwrap()
}

fn image(repository: &str) -> ImageRef {
    format!("ghcr.io/acme/{repository}:main").parse().unwrap()
}

/// The orchestrator as the poller sees it: the tracked services, and every deploy asked for.
/// An accepted deploy makes its services run the deployed digest.
#[derive(Default)]
struct FakeTarget {
    services: Mutex<Vec<TrackedService>>,
    deploys: Mutex<Vec<(String, DigestSet)>>,
    refuse: Mutex<bool>,
}

impl FakeTarget {
    fn track(&self, stack: &str, service: &str, repository: &str, running: Option<u8>) {
        self.services.lock().unwrap().push(TrackedService {
            stack: stack.to_owned(),
            service: service.to_owned(),
            image: image(repository),
            running: running.map(digest),
        });
    }

    /// Sets what a service runs, as a webhook deploy or a rollback would.
    fn run(&self, service: &str, running: u8) {
        for tracked in self.services.lock().unwrap().iter_mut() {
            if tracked.service == service {
                tracked.running = Some(digest(running));
            }
        }
    }

    fn take_deploys(&self) -> Vec<(String, Vec<(String, Digest)>)> {
        std::mem::take(&mut *self.deploys.lock().unwrap())
            .into_iter()
            .map(|(stack, images)| {
                let images = images
                    .into_iter()
                    .map(|(s, d)| (s, d.parse().unwrap()))
                    .collect();
                (stack, images)
            })
            .collect()
    }
}

#[async_trait]
impl PollTarget for Arc<FakeTarget> {
    async fn tracked(&self) -> anyhow::Result<Vec<TrackedService>> {
        Ok(self.services.lock().unwrap().clone())
    }

    async fn deploy(&self, stack: &str, images: DigestSet) -> anyhow::Result<()> {
        if *self.refuse.lock().unwrap() {
            anyhow::bail!("stack {stack} is being removed");
        }
        for (service, to) in &images {
            for tracked in self.services.lock().unwrap().iter_mut() {
                if tracked.stack == stack && &tracked.service == service {
                    tracked.running = Some(to.parse().unwrap());
                }
            }
        }
        self.deploys
            .lock()
            .unwrap()
            .push((stack.to_owned(), images));
        Ok(())
    }
}

/// What the registry serves for each tag, by repository, and the lookups made.
#[derive(Default)]
struct FakeRegistry {
    tags: Mutex<BTreeMap<String, Result<u8, Option<Duration>>>>,
    lookups: Mutex<Vec<String>>,
}

impl FakeRegistry {
    fn serve(&self, repository: &str, n: u8) {
        self.tags
            .lock()
            .unwrap()
            .insert(repository.to_owned(), Ok(n));
    }

    /// Lookups of the repository's tag fail: rate-limited with `retry_after`, or a `500`.
    fn fail(&self, repository: &str, retry_after: Option<Duration>) {
        self.tags
            .lock()
            .unwrap()
            .insert(repository.to_owned(), Err(retry_after));
    }

    fn take_lookups(&self) -> Vec<String> {
        std::mem::take(&mut *self.lookups.lock().unwrap())
    }
}

#[async_trait]
impl TagResolver for FakeRegistry {
    async fn resolve(&self, image: &ImageRef) -> Result<crate::stack::PinnedImage, RegistryError> {
        self.lookups.lock().unwrap().push(image.repository().into());
        let repository = image.repository().trim_start_matches("acme/");
        match self.tags.lock().unwrap().get(repository).copied() {
            Some(Ok(n)) => Ok(image.clone().pin(digest(n))),
            Some(Err(Some(retry_after))) => Err(RegistryError::RateLimited {
                registry: "ghcr.io".into(),
                retry_after: Some(retry_after),
            }),
            Some(Err(None)) => Err(RegistryError::Status {
                registry: "ghcr.io".into(),
                what: "the manifest request",
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            }),
            None => Err(RegistryError::NotFound {
                image: image.to_string(),
            }),
        }
    }

    async fn contains(&self, _: &ImageRef, _: &Digest) -> Result<(), RegistryError> {
        Ok(())
    }
}

#[derive(Default)]
struct MemoryStore(Mutex<Observed>);

#[async_trait]
impl ObservedStore for MemoryStore {
    async fn load(&self) -> anyhow::Result<Observed> {
        Ok(self.0.lock().unwrap().clone())
    }

    async fn save(&self, observed: &Observed) -> anyhow::Result<()> {
        *self.0.lock().unwrap() = observed.clone();
        Ok(())
    }
}

struct Fixture {
    target: Arc<FakeTarget>,
    registry: Arc<FakeRegistry>,
    store: Arc<MemoryStore>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            target: Arc::default(),
            registry: Arc::default(),
            store: Arc::default(),
        }
    }

    /// A poller over the fixture's fakes, as after a (re)start, without jitter.
    fn poller(&self) -> Poller {
        Poller::new(
            Box::new(Arc::clone(&self.target)),
            Arc::clone(&self.registry) as Arc<dyn TagResolver>,
            Arc::clone(&self.store) as Arc<dyn ObservedStore>,
            &PollerConfig { interval: INTERVAL },
        )
        .with_jitter(Duration::ZERO)
    }

    fn observed(&self, repository: &str) -> Option<Digest> {
        self.store
            .0
            .lock()
            .unwrap()
            .get(&image(repository).to_string())
            .map(|o| o.digest.parse().unwrap())
    }
}

fn deploy(stack: &str, images: &[(&str, u8)]) -> (String, Vec<(String, Digest)>) {
    let images = images
        .iter()
        .map(|(s, n)| ((*s).to_owned(), digest(*n)))
        .collect();
    (stack.to_owned(), images)
}

#[tokio::test(start_paused = true)]
async fn first_sighting_of_a_never_run_service_deploys_it() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", None);
    f.registry.serve("notes-api", 1);
    let mut poller = f.poller();

    poller.poll().await.unwrap();

    assert_eq!(
        f.target.take_deploys(),
        vec![deploy("notes", &[("api", 1)])]
    );
    assert_eq!(f.observed("notes-api"), Some(digest(1)));
    // The next poll sees the same digest, now running: nothing to do.
    poller.poll().await.unwrap();
    assert!(f.target.take_deploys().is_empty());
}

#[tokio::test(start_paused = true)]
async fn first_sighting_of_a_running_service_only_records_the_digest() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", Some(1));
    f.registry.serve("notes-api", 2);

    f.poller().poll().await.unwrap();

    assert!(f.target.take_deploys().is_empty());
    assert_eq!(f.observed("notes-api"), Some(digest(2)));
}

#[tokio::test(start_paused = true)]
async fn a_changed_digest_deploys_and_an_unchanged_one_does_nothing() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", Some(1));
    f.registry.serve("notes-api", 1);
    let mut poller = f.poller();
    poller.poll().await.unwrap();

    // No change: nothing.
    poller.poll().await.unwrap();
    assert!(f.target.take_deploys().is_empty());

    // The tag moves.
    f.registry.serve("notes-api", 2);
    poller.poll().await.unwrap();
    assert_eq!(
        f.target.take_deploys(),
        vec![deploy("notes", &[("api", 2)])]
    );
    assert_eq!(f.observed("notes-api"), Some(digest(2)));

    poller.poll().await.unwrap();
    assert!(f.target.take_deploys().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_rollback_holds_until_the_tag_moves_again() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", Some(1));
    f.registry.serve("notes-api", 1);
    let mut poller = f.poller();
    poller.poll().await.unwrap();
    f.registry.serve("notes-api", 2);
    poller.poll().await.unwrap();
    assert_eq!(
        f.target.take_deploys(),
        vec![deploy("notes", &[("api", 2)])]
    );

    // The admin rolls back to digest 1; the tag still points at 2.
    f.target.run("api", 1);
    poller.poll().await.unwrap();
    poller.poll().await.unwrap();
    assert!(f.target.take_deploys().is_empty(), "the rollback holds");

    // A restart does not undo it either: the observation is persisted.
    f.poller().poll().await.unwrap();
    assert!(f.target.take_deploys().is_empty(), "the rollback holds");

    // The next release moves the tag.
    f.registry.serve("notes-api", 3);
    poller.poll().await.unwrap();
    assert_eq!(
        f.target.take_deploys(),
        vec![deploy("notes", &[("api", 3)])]
    );
}

#[tokio::test(start_paused = true)]
async fn a_digest_the_webhook_already_deployed_is_not_deployed_again() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", Some(1));
    f.registry.serve("notes-api", 1);
    let mut poller = f.poller();
    poller.poll().await.unwrap();

    // The release's webhook deploys digest 2 before the poller notices the tag moved.
    f.target.run("api", 2);
    f.registry.serve("notes-api", 2);
    poller.poll().await.unwrap();

    assert!(f.target.take_deploys().is_empty());
    assert_eq!(f.observed("notes-api"), Some(digest(2)));
}

#[tokio::test(start_paused = true)]
async fn a_never_run_service_that_starts_tracking_a_known_tag_is_deployed() {
    let f = Fixture::new();
    f.target.track("notes", "api", "shared", Some(1));
    f.registry.serve("shared", 1);
    let mut poller = f.poller();
    poller.poll().await.unwrap();
    assert!(f.target.take_deploys().is_empty());

    // A second stack tracks the same tag, and a worker on the same image joins the first one.
    f.target.track("blog", "api", "shared", None);
    f.target.track("notes", "worker", "shared", None);
    poller.poll().await.unwrap();
    assert_eq!(
        f.target.take_deploys(),
        vec![
            deploy("blog", &[("api", 1)]),
            deploy("notes", &[("worker", 1)])
        ]
    );

    poller.poll().await.unwrap();
    assert!(f.target.take_deploys().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failed_first_deploy_waits_for_the_tag_to_move() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", None);
    f.registry.serve("notes-api", 1);
    let mut poller = f.poller();
    poller.poll().await.unwrap();
    assert_eq!(
        f.target.take_deploys(),
        vec![deploy("notes", &[("api", 1)])]
    );

    // The deploy was accepted but failed its health check: the service still never ran.
    f.target.services.lock().unwrap()[0].running = None;
    poller.poll().await.unwrap();
    f.poller().poll().await.unwrap();
    assert!(f.target.take_deploys().is_empty(), "no retry loop");

    f.registry.serve("notes-api", 2);
    poller.poll().await.unwrap();
    assert_eq!(
        f.target.take_deploys(),
        vec![deploy("notes", &[("api", 2)])]
    );
}

#[tokio::test(start_paused = true)]
async fn services_of_one_stack_are_deployed_in_one_request() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", None);
    f.target.track("notes", "web", "notes-web", None);
    f.target.track("blog", "app", "blog", Some(7));
    f.registry.serve("notes-api", 1);
    f.registry.serve("notes-web", 2);
    f.registry.serve("blog", 7);

    f.poller().poll().await.unwrap();

    assert_eq!(
        f.target.take_deploys(),
        vec![deploy("notes", &[("api", 1), ("web", 2)])]
    );
}

#[tokio::test(start_paused = true)]
async fn a_tag_tracked_by_several_services_is_looked_up_once() {
    let f = Fixture::new();
    f.target.track("notes", "api", "shared", Some(1));
    f.target.track("blog", "api", "shared", Some(1));
    f.registry.serve("shared", 1);
    let mut poller = f.poller();
    poller.poll().await.unwrap();
    assert_eq!(f.registry.take_lookups(), vec!["acme/shared"]);

    f.registry.serve("shared", 2);
    poller.poll().await.unwrap();
    assert_eq!(
        f.target.take_deploys(),
        vec![
            deploy("blog", &[("api", 2)]),
            deploy("notes", &[("api", 2)])
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_deploy_is_retried_on_the_next_poll() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", Some(1));
    f.registry.serve("notes-api", 1);
    let mut poller = f.poller();
    poller.poll().await.unwrap();

    f.registry.serve("notes-api", 2);
    *f.target.refuse.lock().unwrap() = true;
    poller.poll().await.unwrap();
    assert_eq!(f.observed("notes-api"), Some(digest(1)), "not recorded yet");

    *f.target.refuse.lock().unwrap() = false;
    poller.poll().await.unwrap();
    assert_eq!(
        f.target.take_deploys(),
        vec![deploy("notes", &[("api", 2)])]
    );
    assert_eq!(f.observed("notes-api"), Some(digest(2)));
}

#[tokio::test(start_paused = true)]
async fn observations_of_untracked_tags_are_dropped() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", Some(1));
    f.registry.serve("notes-api", 1);
    let mut poller = f.poller();
    poller.poll().await.unwrap();
    assert!(f.observed("notes-api").is_some());

    // The stack is removed, then applied again: its service has not run since.
    f.target.services.lock().unwrap().clear();
    poller.poll().await.unwrap();
    assert_eq!(f.observed("notes-api"), None);
    f.target.track("notes", "api", "notes-api", None);
    poller.poll().await.unwrap();
    assert_eq!(
        f.target.take_deploys(),
        vec![deploy("notes", &[("api", 1)])]
    );
}

#[tokio::test(start_paused = true)]
async fn failing_lookups_back_off_per_image() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", Some(1));
    f.target.track("blog", "app", "blog", Some(1));
    f.registry.fail("notes-api", None);
    f.registry.serve("blog", 1);
    let mut poller = f.poller();

    // Failures 1, 2 and 3 wait 1, 2 and 4 intervals.
    let mut notes_lookups = Vec::new();
    for _ in 0..8 {
        poller.poll().await.unwrap();
        let lookups = f.registry.take_lookups();
        assert!(
            lookups.contains(&"acme/blog".to_owned()),
            "blog never waits"
        );
        notes_lookups.push(lookups.contains(&"acme/notes-api".to_owned()));
        tokio::time::advance(INTERVAL).await;
    }
    assert_eq!(
        notes_lookups,
        [true, true, false, true, false, false, false, true]
    );

    // Recovered: looked up on every poll again, and the tag is observed.
    f.registry.serve("notes-api", 1);
    tokio::time::advance(Poller::MAX_BACKOFF).await;
    poller.poll().await.unwrap();
    poller.poll().await.unwrap();
    assert_eq!(
        f.registry.take_lookups(),
        ["acme/blog", "acme/notes-api", "acme/blog", "acme/notes-api"]
    );
    assert_eq!(f.observed("notes-api"), Some(digest(1)));
}

#[tokio::test(start_paused = true)]
async fn a_rate_limit_waits_at_least_its_retry_after() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", Some(1));
    f.registry
        .fail("notes-api", Some(Duration::from_secs(3 * 3600)));
    let mut poller = f.poller();
    poller.poll().await.unwrap();
    f.registry.take_lookups();
    f.registry.serve("notes-api", 1);

    tokio::time::advance(Duration::from_secs(3 * 3600 - 1)).await;
    poller.poll().await.unwrap();
    assert!(f.registry.take_lookups().is_empty());

    tokio::time::advance(Duration::from_secs(1)).await;
    poller.poll().await.unwrap();
    assert_eq!(f.registry.take_lookups(), ["acme/notes-api"]);
}

#[test]
fn backoff_doubles_up_to_the_maximum() {
    let now = Instant::now();
    let err = RegistryError::NotFound { image: "x".into() };
    let mut previous = None;
    let mut delays = Vec::new();
    for _ in 0..6 {
        let next = Backoff::after(previous.as_ref(), &err, INTERVAL, now);
        delays.push((next.retry_at - now).as_secs() / 60);
        previous = Some(next);
    }
    assert_eq!(delays, [5, 10, 20, 40, 60, 60]);
}

#[tokio::test(start_paused = true)]
async fn run_polls_every_interval_until_cancelled() {
    let f = Fixture::new();
    f.target.track("notes", "api", "notes-api", Some(1));
    f.registry.serve("notes-api", 1);
    let token = CancellationToken::new();
    let task = tokio::spawn(f.poller().run(token.clone()));

    // The first poll runs at once (no jitter), the next ones every interval.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(f.registry.take_lookups().len(), 1);
    tokio::time::sleep(INTERVAL - Duration::from_secs(2)).await;
    assert!(f.registry.take_lookups().is_empty());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(f.registry.take_lookups().len(), 1);
    tokio::time::sleep(INTERVAL * 3).await;
    assert_eq!(f.registry.take_lookups().len(), 3);

    token.cancel();
    task.await.unwrap();
}

#[test]
fn jitter_stays_within_its_bound() {
    let f = Fixture::new();
    let poller = f.poller().with_jitter(Duration::from_secs(30));
    for _ in 0..100 {
        assert!(poller.random_jitter() <= Duration::from_secs(30));
    }
    assert_eq!(f.poller().random_jitter(), Duration::ZERO);
}
