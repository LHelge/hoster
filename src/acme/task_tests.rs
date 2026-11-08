//! The ACME task against a fake CA, a fake resolver and an in-memory backoff store, on paused
//! tokio time: a [`FakeClock`] derives wall-clock time from tokio's clock, so certificate
//! lifetimes and backoffs advance with it.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::task::JoinHandle;

use super::*;
use crate::acme::IssueFailure;

const ADMIN: &str = "admin.example.com";
const HOSTER_IP: &str = "192.0.2.1";
const CDN_IP: &str = "203.0.113.9";
const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const HOUR: Duration = Duration::from_secs(60 * 60);
const MINUTE: Duration = Duration::from_secs(60);

/// Wall-clock time that moves with tokio's (paused) clock.
struct FakeClock {
    base: SystemTime,
    start: tokio::time::Instant,
}

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            base: SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            start: tokio::time::Instant::now(),
        })
    }

    /// How long after the start `at` is.
    fn since_start(&self, at: SystemTime) -> Duration {
        at.duration_since(self.base).unwrap()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> SystemTime {
        self.base + self.start.elapsed()
    }
}

/// A CA that issues certificates of a fixed lifetime instantly, except for failing hosts.
struct FakeAcme {
    clock: Arc<FakeClock>,
    lifetime: Duration,
    state: Mutex<FakeAcmeState>,
}

#[derive(Default)]
struct FakeAcmeState {
    certs: HashMap<String, Validity>,
    failing: BTreeSet<String>,
    windows: HashMap<String, RenewalWindow>,
    /// Every issuance attempt: host and time since the start.
    attempts: Vec<(String, Duration)>,
}

impl FakeAcme {
    fn new(clock: Arc<FakeClock>, lifetime: Duration) -> Arc<Self> {
        Arc::new(Self {
            clock,
            lifetime,
            state: Mutex::default(),
        })
    }

    fn fail(&self, host: &str, failing: bool) {
        let mut state = self.state.lock().unwrap();
        if failing {
            state.failing.insert(host.to_owned());
        } else {
            state.failing.remove(host);
        }
    }

    fn window(&self, host: &str, window: RenewalWindow) {
        self.state
            .lock()
            .unwrap()
            .windows
            .insert(host.to_owned(), window);
    }

    /// The attempts for `host`, as times since the start.
    fn attempts(&self, host: &str) -> Vec<Duration> {
        let state = self.state.lock().unwrap();
        state
            .attempts
            .iter()
            .filter(|(h, _)| h == host)
            .map(|(_, at)| *at)
            .collect()
    }
}

#[async_trait]
impl Acme for FakeAcme {
    fn validity(&self, host: &str) -> Option<Validity> {
        self.state.lock().unwrap().certs.get(host).copied()
    }

    async fn renewal_window(&self, host: &str) -> Option<RenewalWindow> {
        self.state.lock().unwrap().windows.get(host).copied()
    }

    async fn issue(&self, host: &str) -> Result<Validity, IssueError> {
        let now = self.clock.now();
        let mut state = self.state.lock().unwrap();
        state
            .attempts
            .push((host.to_owned(), self.clock.since_start(now)));
        if state.failing.contains(host) {
            return Err(IssueError {
                host: host.to_owned(),
                failure: IssueFailure::Validation {
                    detail: "connection refused".into(),
                },
            });
        }
        let validity = Validity {
            not_before: now,
            not_after: now + self.lifetime,
        };
        state.certs.insert(host.to_owned(), validity);
        state.windows.remove(host);
        Ok(validity)
    }
}

/// Fixed A/AAAA records; unknown names do not resolve.
#[derive(Default)]
struct FakeResolver {
    records: HashMap<String, Vec<IpAddr>>,
}

impl FakeResolver {
    fn with(mut self, host: &str, ip: &str) -> Self {
        self.records
            .entry(host.to_owned())
            .or_default()
            .push(ip.parse().unwrap());
        self
    }
}

#[async_trait]
impl Resolve for FakeResolver {
    async fn lookup(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        self.records
            .get(host)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "NXDOMAIN"))
    }
}

/// The backoffs in memory, shared across task restarts.
#[derive(Default)]
struct MemoryStore {
    backoffs: Mutex<Backoffs>,
}

#[async_trait]
impl BackoffStore for MemoryStore {
    async fn load(&self) -> anyhow::Result<Backoffs> {
        Ok(self.backoffs.lock().unwrap().clone())
    }

    async fn save(&self, backoffs: &Backoffs) -> anyhow::Result<()> {
        *self.backoffs.lock().unwrap() = backoffs.clone();
        Ok(())
    }
}

/// A running task and its fakes.
struct Harness {
    clock: Arc<FakeClock>,
    acme: Arc<FakeAcme>,
    store: Arc<MemoryStore>,
    commands: mpsc::UnboundedSender<AcmeCommand>,
    events: mpsc::Receiver<AcmeEvent>,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl Harness {
    fn default_resolver() -> FakeResolver {
        FakeResolver::default()
            .with(ADMIN, HOSTER_IP)
            .with("notes.example.com", HOSTER_IP)
            .with("cdn.example.com", CDN_IP)
    }

    /// A task with certificates of `lifetime`, the admin host's addresses as public addresses.
    fn start(lifetime: Duration) -> Self {
        let clock = FakeClock::new();
        let acme = FakeAcme::new(Arc::clone(&clock), lifetime);
        Self::spawn(
            clock,
            acme,
            Arc::default(),
            PublicAddresses::OfHost(ADMIN.into()),
            Self::default_resolver(),
        )
    }

    fn spawn(
        clock: Arc<FakeClock>,
        acme: Arc<FakeAcme>,
        store: Arc<MemoryStore>,
        public: PublicAddresses,
        resolver: FakeResolver,
    ) -> Self {
        let (commands, rx) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::channel(64);
        let shutdown = CancellationToken::new();
        let task = AcmeTask::new(
            Arc::clone(&acme) as Arc<dyn Acme>,
            Arc::clone(&store) as Arc<dyn BackoffStore>,
            ADMIN,
            public,
            events_tx,
        )
        .with_resolver(Arc::new(resolver))
        .with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        let task = tokio::spawn(task.run(rx, shutdown.clone()));
        Self {
            clock,
            acme,
            store,
            commands,
            events,
            shutdown,
            task,
        }
    }

    /// Stops the task and starts a new one with the same CA, store and clock.
    async fn restart(self) -> Self {
        self.shutdown.cancel();
        self.task.await.unwrap();
        Self::spawn(
            self.clock,
            self.acme,
            self.store,
            PublicAddresses::OfHost(ADMIN.into()),
            Self::default_resolver(),
        )
    }

    fn ensure(&self, hosts: &[&str]) {
        let hosts = hosts.iter().map(|h| (*h).to_owned()).collect();
        self.commands.send(AcmeCommand::Ensure { hosts }).unwrap();
    }

    fn forget(&self, host: &str) {
        let host = host.to_owned();
        self.commands.send(AcmeCommand::Forget { host }).unwrap();
    }

    async fn next(&mut self) -> AcmeEvent {
        self.events.recv().await.expect("the task is running")
    }

    /// Lets (paused) time pass.
    async fn pass(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    /// The events sent so far.
    fn sent(&mut self) -> Vec<AcmeEvent> {
        std::iter::from_fn(|| self.events.try_recv().ok()).collect()
    }
}

fn issued(event: &AcmeEvent) -> Option<&str> {
    match event {
        AcmeEvent::Issued { host, .. } => Some(host),
        _ => None,
    }
}

#[tokio::test(start_paused = true)]
async fn the_admin_host_is_issued_at_startup_and_ensure_issues_each_missing_host_once() {
    let mut h = Harness::start(90 * DAY);
    let event = h.next().await;
    assert_eq!(issued(&event), Some(ADMIN), "{event:?}");

    // Duplicates in one command, a host that already has a certificate, case differences.
    h.ensure(&["notes.example.com", "NOTES.example.com", ADMIN]);
    let event = h.next().await;
    assert_eq!(issued(&event), Some("notes.example.com"), "{event:?}");
    h.ensure(&["notes.example.com"]);
    h.pass(MINUTE).await;
    assert_eq!(h.sent(), []);
    assert_eq!(h.acme.attempts("notes.example.com"), [Duration::ZERO]);
    assert_eq!(h.acme.attempts(ADMIN), [Duration::ZERO]);
}

#[tokio::test(start_paused = true)]
async fn certificates_renew_once_a_third_of_their_lifetime_remains() {
    // 30 h certificates are due after 20 h; the hourly tick finds them.
    let mut h = Harness::start(30 * HOUR);
    h.ensure(&["notes.example.com"]);
    h.pass(19 * HOUR + 30 * MINUTE).await;
    assert_eq!(h.acme.attempts("notes.example.com").len(), 1);

    h.pass(HOUR).await;
    assert_eq!(
        h.acme.attempts("notes.example.com"),
        [Duration::ZERO, 20 * HOUR]
    );
    assert_eq!(h.acme.attempts(ADMIN), [Duration::ZERO, 20 * HOUR]);
    let renewed: Vec<_> = h
        .sent()
        .iter()
        .filter_map(issued)
        .map(String::from)
        .collect();
    assert_eq!(renewed.len(), 4, "{renewed:?}");
}

#[tokio::test(start_paused = true)]
async fn certificates_renew_early_when_the_ari_window_opens() {
    let mut h = Harness::start(90 * DAY);
    h.next().await;
    let start = h.clock.now();
    h.acme.window(
        ADMIN,
        RenewalWindow {
            start: start + 5 * HOUR + 30 * MINUTE,
            end: start + 7 * HOUR,
        },
    );
    h.pass(5 * HOUR + 45 * MINUTE).await;
    assert_eq!(h.acme.attempts(ADMIN).len(), 1);
    let event = h.next().await;
    assert_eq!(issued(&event), Some(ADMIN));
    assert_eq!(h.acme.attempts(ADMIN), [Duration::ZERO, 6 * HOUR]);
}

#[tokio::test(start_paused = true)]
async fn failures_back_off_exponentially_up_to_a_day() {
    let mut h = Harness::start(90 * DAY);
    h.acme.fail("notes.example.com", true);
    h.ensure(&["notes.example.com"]);

    let mut retries = Vec::new();
    while retries.len() < 11 {
        if let AcmeEvent::Failed {
            host,
            retry_at,
            error,
        } = h.next().await
        {
            assert_eq!(host, "notes.example.com");
            assert!(error.contains("connection refused"), "{error}");
            retries.push(retry_at);
        }
    }
    let attempts = h.acme.attempts("notes.example.com");
    let gaps: Vec<u64> = attempts
        .windows(2)
        .map(|w| (w[1] - w[0]).as_secs() / 60)
        .collect();
    assert_eq!(gaps, [5, 10, 20, 40, 80, 160, 320, 640, 1280, 1440]);
    // Each attempt is at the time the previous failure announced.
    for (attempt, retry_at) in attempts.iter().skip(1).zip(&retries) {
        assert_eq!(*attempt, h.clock.since_start(*retry_at));
    }

    // Once the CA succeeds, the backoff is gone.
    h.acme.fail("notes.example.com", false);
    let event = loop {
        let event = h.next().await;
        if issued(&event).is_some() {
            break event;
        }
    };
    assert_eq!(issued(&event), Some("notes.example.com"));
    assert!(h.store.backoffs.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_backoff_survives_a_restart() {
    let mut h = Harness::start(90 * DAY);
    h.next().await;
    h.acme.fail("notes.example.com", true);
    h.ensure(&["notes.example.com"]);
    h.next().await;
    h.next().await; // the retry after 5 min
    let persisted = h.store.backoffs.lock().unwrap()["notes.example.com"];
    assert_eq!(persisted.failures, 2);

    // After the restart the orchestrator ensures the host again at once: no attempt until the
    // persisted retry time, then the backoff continues where it left off.
    let mut h = h.restart().await;
    h.ensure(&["notes.example.com"]);
    h.pass(MINUTE).await;
    assert_eq!(h.acme.attempts("notes.example.com").len(), 2);
    let AcmeEvent::Failed { retry_at, .. } = h.next().await else {
        panic!("expected a failure");
    };
    let attempts = h.acme.attempts("notes.example.com");
    assert_eq!(attempts, [Duration::ZERO, 5 * MINUTE, 15 * MINUTE]);
    assert_eq!(h.clock.since_start(retry_at), 35 * MINUTE);
}

#[tokio::test(start_paused = true)]
async fn a_dns_mismatch_is_reported_and_issuance_still_attempted() {
    let mut h = Harness::start(90 * DAY);
    h.next().await;

    // Behind a CDN: the host resolves elsewhere.
    h.ensure(&["cdn.example.com"]);
    assert_eq!(
        h.next().await,
        AcmeEvent::DnsMismatch(DnsMismatch {
            host: "cdn.example.com".into(),
            observed: vec![CDN_IP.parse().unwrap()],
            expected: vec![HOSTER_IP.parse().unwrap()],
        })
    );
    assert_eq!(issued(&h.next().await), Some("cdn.example.com"));

    // No records at all, and the CA fails too: both are reported.
    h.acme.fail("typo.example.com", true);
    h.ensure(&["typo.example.com"]);
    let AcmeEvent::DnsMismatch(mismatch) = h.next().await else {
        panic!("expected a DNS mismatch");
    };
    assert!(mismatch.observed.is_empty());
    assert!(matches!(h.next().await, AcmeEvent::Failed { .. }));

    // A host pointing here is not reported.
    h.ensure(&["notes.example.com"]);
    assert_eq!(issued(&h.next().await), Some("notes.example.com"));
}

#[tokio::test(start_paused = true)]
async fn configured_public_addresses_override_the_admin_hosts_records() {
    let clock = FakeClock::new();
    let acme = FakeAcme::new(Arc::clone(&clock), 90 * DAY);
    let public = PublicAddresses::Configured(BTreeSet::from([CDN_IP.parse().unwrap()]));
    let mut h = Harness::spawn(
        clock,
        acme,
        Arc::default(),
        public,
        Harness::default_resolver(),
    );
    // The admin host itself now mismatches; the CDN host does not.
    assert!(matches!(h.next().await, AcmeEvent::DnsMismatch(m) if m.host == ADMIN));
    assert_eq!(issued(&h.next().await), Some(ADMIN));
    h.ensure(&["cdn.example.com"]);
    assert_eq!(issued(&h.next().await), Some("cdn.example.com"));
}

#[tokio::test(start_paused = true)]
async fn forgotten_hosts_are_not_renewed_or_retried_but_the_admin_host_stays() {
    let mut h = Harness::start(30 * HOUR);
    h.acme.fail("typo.example.com", true);
    h.ensure(&["notes.example.com", "typo.example.com"]);
    h.pass(MINUTE).await;
    h.forget("notes.example.com");
    h.forget("typo.example.com");
    h.forget(ADMIN);
    h.pass(2 * DAY).await;

    assert_eq!(h.acme.attempts("notes.example.com"), [Duration::ZERO]);
    assert_eq!(h.acme.attempts("typo.example.com"), [Duration::ZERO]);
    assert!(h.acme.attempts(ADMIN).len() > 1, "the admin host renews");
    assert!(h.store.backoffs.lock().unwrap().is_empty());
    h.sent();
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_the_task() {
    let h = Harness::start(90 * DAY);
    h.shutdown.cancel();
    h.task.await.unwrap();
}

#[test]
fn backoff_doubles_from_five_minutes_to_a_day() {
    let delays: Vec<u64> = (1..=12).map(|n| Backoff::delay(n).as_secs() / 60).collect();
    assert_eq!(
        delays,
        [5, 10, 20, 40, 80, 160, 320, 640, 1280, 1440, 1440, 1440]
    );
    assert_eq!(Backoff::delay(u32::MAX), Backoff::MAX);

    let now = SystemTime::UNIX_EPOCH;
    let first = Backoff::after(None, now);
    assert_eq!((first.failures, first.retry_at), (1, now + 5 * MINUTE));
    let second = Backoff::after(Some(&first), now);
    assert_eq!((second.failures, second.retry_at), (2, now + 10 * MINUTE));
}

#[test]
fn validity_is_due_for_renewal_with_a_third_of_its_lifetime_left() {
    let not_before = SystemTime::UNIX_EPOCH;
    let validity = Validity {
        not_before,
        not_after: not_before + 90 * DAY,
    };
    assert_eq!(validity.renew_at(), not_before + 60 * DAY);
}

#[tokio::test]
async fn backoffs_persist_in_the_state_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();
    assert!(BackoffStore::load(&store).await.unwrap().is_empty());

    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let backoffs = Backoffs::from([(
        "notes.example.com".to_owned(),
        Backoff {
            failures: 3,
            retry_at: at,
        },
    )]);
    BackoffStore::save(&store, &backoffs).await.unwrap();
    assert_eq!(BackoffStore::load(&store).await.unwrap(), backoffs);
    BackoffStore::save(&store, &Backoffs::new()).await.unwrap();
    assert!(store.kv_get(BACKOFF_KEY).await.unwrap().is_none());
}
