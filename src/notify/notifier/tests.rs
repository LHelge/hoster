//! The notifier with a real store and a fake provider.

use std::collections::VecDeque;
use std::sync::Mutex;

use reqwest::StatusCode;
use tempfile::TempDir;

use super::*;
use crate::secrets::SecretKey;
use crate::store::{EventKind, EventQuery, NewEvent, NotificationSettings, Severity};

/// A provider that records what it is asked to send and answers from a script, then `Ok`.
#[derive(Debug, Default)]
struct FakeProvider {
    sent: Mutex<Vec<Message>>,
    answers: Mutex<VecDeque<Result<Delivery, DeliveryError>>>,
    /// Answer every request after the script with this, instead of `Ok`.
    always_fail: Mutex<Option<fn() -> DeliveryError>>,
}

impl FakeProvider {
    fn answering(answers: impl IntoIterator<Item = Result<Delivery, DeliveryError>>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into_iter().collect()),
            ..Self::default()
        })
    }

    fn failing(error: fn() -> DeliveryError) -> Arc<Self> {
        Arc::new(Self {
            always_fail: Mutex::new(Some(error)),
            ..Self::default()
        })
    }

    fn subjects(&self) -> Vec<String> {
        let sent = self.sent.lock().unwrap();
        sent.iter().map(|m| m.subject.clone()).collect()
    }

    fn attempts(&self) -> usize {
        self.sent.lock().unwrap().len()
    }
}

impl Provider for FakeProvider {
    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<Delivery, DeliveryError>> {
        self.sent.lock().unwrap().push(message.clone());
        let answer = self.answers.lock().unwrap().pop_front();
        let result = match answer {
            Some(answer) => answer,
            None => match *self.always_fail.lock().unwrap() {
                Some(error) => Err(error()),
                None => Ok(Delivery {
                    id: Some(format!("mail-{}", self.attempts())),
                }),
            },
        };
        async move { result }.boxed()
    }
}

/// Hands out the one fake provider.
struct FakeMailer(Arc<FakeProvider>);

impl Mailer for FakeMailer {
    fn provider(
        &self,
        _kind: ProviderKind,
    ) -> BoxFuture<'_, Result<Arc<dyn Provider>, DeliveryError>> {
        let provider: Arc<dyn Provider> = self.0.clone();
        async move { Ok(provider) }.boxed()
    }
}

fn unavailable() -> DeliveryError {
    DeliveryError::Unavailable {
        provider: ProviderKind::Resend,
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: None,
        message: Some("try later".into()),
        retry_after: None,
    }
}

fn rejected() -> DeliveryError {
    DeliveryError::Rejected {
        provider: ProviderKind::Resend,
        status: StatusCode::FORBIDDEN,
        code: Some("validation_error".into()),
        message: Some("API key is invalid".into()),
    }
}

/// Short waits, so the retry tests take milliseconds.
const FAST: Retry = Retry {
    first: Duration::from_millis(1),
    max: Duration::from_millis(2),
    window: Duration::from_millis(50),
};

struct Harness {
    _dir: TempDir,
    store: Store,
    log: EventLog,
    ui_events: broadcast::Sender<UiEvent>,
    instance: Instance,
}

impl Harness {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let (ui_events, _) = broadcast::channel(16);
        let log = EventLog::detached(store.clone(), ui_events.clone());
        let instance = Instance::with_default(store.clone(), "srv-01".parse().unwrap())
            .await
            .unwrap();
        Self {
            _dir: dir,
            store,
            log,
            ui_events,
            instance,
        }
    }

    /// The harness with notifications switched on.
    async fn enabled() -> Self {
        let harness = Self::new().await;
        harness.enable(true).await;
        harness
    }

    async fn enable(&self, enabled: bool) {
        self.store
            .set_notification_settings(&NotificationSettings {
                provider: ProviderKind::Resend,
                from: Some("hoster@example.com".parse().unwrap()),
                recipient: Some("ops@example.com".parse().unwrap()),
                enabled,
                also_successes: false,
            })
            .await
            .unwrap();
    }

    fn notifier_with(&self, mailer: Arc<dyn Mailer>) -> Notifier {
        Notifier::new(
            self.store.clone(),
            self.log.clone(),
            mailer,
            self.instance.clone(),
            "https://hoster.example.com".parse().unwrap(),
            self.ui_events.subscribe(),
        )
        .with_timing(FAST, Duration::from_secs(3600))
    }

    fn notifier(&self, provider: &Arc<FakeProvider>) -> Notifier {
        self.notifier_with(Arc::new(FakeMailer(provider.clone())))
    }

    async fn record(&self, event: NewEvent) {
        self.log.record(event).await;
    }

    async fn failure_events(&self) -> Vec<Event> {
        let query = EventQuery {
            kinds: [EventKind::NotificationFailed].into(),
            ..EventQuery::default()
        };
        self.store.event_page(&query).await.unwrap().events
    }

    async fn newest_id(&self) -> crate::store::EventId {
        let page = self.store.event_page(&EventQuery::default()).await.unwrap();
        page.events[0].id
    }
}

fn crash_loop(service: &str) -> NewEvent {
    NewEvent::new(
        EventKind::CrashLoopEntered,
        Severity::Error,
        format!("notes/{service} is crash looping"),
    )
    .stack("notes")
    .subject("/stacks/notes")
}

fn crash_loop_left(service: &str) -> NewEvent {
    NewEvent::new(
        EventKind::CrashLoopLeft,
        Severity::Success,
        format!("notes/{service} stopped crash looping"),
    )
    .stack("notes")
    .subject("/stacks/notes")
}

async fn pass(notifier: &mut Notifier) {
    notifier.pass(&CancellationToken::new()).await.unwrap();
}

#[tokio::test]
async fn a_problem_then_its_recovery_send_one_mail_each() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::answering([]);
    let mut notifier = h.notifier(&provider);

    h.record(crash_loop("web")).await;
    h.record(crash_loop("web")).await;
    pass(&mut notifier).await;
    assert_eq!(provider.subjects(), ["[srv-01] notes/web is crash looping"]);
    assert_eq!(h.store.notified_problems().await.unwrap().len(), 1);
    let message = provider.sent.lock().unwrap()[0].clone();
    assert_eq!(message.from.as_ref(), "hoster@example.com");
    assert_eq!(message.to.as_ref(), "ops@example.com");
    assert!(
        message
            .idempotency_key
            .as_ref()
            .starts_with("hoster-event-")
    );

    h.record(crash_loop_left("web")).await;
    pass(&mut notifier).await;
    assert_eq!(
        provider.subjects()[1..],
        ["[srv-01] notes/web stopped crash looping"]
    );
    assert!(h.store.notified_problems().await.unwrap().is_empty());
    assert_eq!(
        h.store.notifier_cursor().await.unwrap(),
        h.newest_id().await
    );
    let last = h.store.last_delivery().await.unwrap().unwrap();
    assert_eq!(
        last.outcome,
        DeliveryOutcome::Sent {
            id: Some("mail-2".into())
        }
    );
}

#[tokio::test]
async fn subjects_use_the_current_instance_name() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::answering([]);
    let mut notifier = h.notifier(&provider);
    h.instance.set(&"prod-eu".parse().unwrap()).await.unwrap();
    h.record(crash_loop("web")).await;
    pass(&mut notifier).await;
    assert_eq!(
        provider.subjects(),
        ["[prod-eu] notes/web is crash looping"]
    );
}

#[tokio::test]
async fn a_retryable_error_is_retried_until_it_succeeds() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::answering([Err(unavailable()), Err(unavailable())]);
    let mut notifier = h.notifier(&provider);
    h.record(crash_loop("web")).await;
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), 3);
    let keys: Vec<_> = provider
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.idempotency_key.clone())
        .collect();
    assert!(
        keys.windows(2).all(|w| w[0] == w[1]),
        "one key for every try"
    );
    assert_eq!(h.store.notified_problems().await.unwrap().len(), 1);
    assert!(h.failure_events().await.is_empty());
}

#[tokio::test]
async fn a_retryable_error_past_the_window_drops_the_mail_and_records_the_failure() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::failing(unavailable);
    let mut notifier = h.notifier(&provider);
    h.record(crash_loop("web")).await;
    pass(&mut notifier).await;
    assert!(provider.attempts() > 1, "retried");
    // Not mailed, so not recorded: a repeat is mailed as new.
    assert!(h.store.notified_problems().await.unwrap().is_empty());
    let failures = h.failure_events().await;
    assert_eq!(failures.len(), 1);
    assert_eq!(
        failures[0].meta.as_deref(),
        Some("Resend answered 503 Service Unavailable: try later")
    );
    let last = h.store.last_delivery().await.unwrap().unwrap();
    assert!(matches!(last.outcome, DeliveryOutcome::Failed { .. }));
    // The failure event itself is read past and never mailed.
    let attempts = provider.attempts();
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), attempts);
    assert_eq!(
        h.store.notifier_cursor().await.unwrap(),
        h.newest_id().await
    );
}

#[tokio::test]
async fn a_permanent_error_drops_at_once_and_writes_one_failure_event_an_hour() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::failing(rejected);
    let mut notifier = h.notifier(&provider);
    h.record(crash_loop("web")).await;
    h.record(crash_loop("worker")).await;
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), 2, "one try each, no retry");
    let failures = h.failure_events().await;
    assert_eq!(failures.len(), 1, "not one per event");
    assert!(
        failures[0]
            .meta
            .as_deref()
            .unwrap()
            .contains("API key is invalid")
    );
}

#[tokio::test]
async fn a_restart_between_events_keeps_the_cursor_and_the_open_problems() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::answering([]);
    h.record(crash_loop("web")).await;
    pass(&mut h.notifier(&provider)).await;

    // A new notifier, as after a restart: nothing is sent twice, the repeat is held back
    // and the recovery is mailed.
    let mut restarted = h.notifier(&provider);
    pass(&mut restarted).await;
    h.record(crash_loop("web")).await;
    pass(&mut restarted).await;
    assert_eq!(provider.attempts(), 1);
    h.record(crash_loop_left("web")).await;
    pass(&mut restarted).await;
    assert_eq!(
        provider.subjects(),
        [
            "[srv-01] notes/web is crash looping",
            "[srv-01] notes/web stopped crash looping"
        ]
    );
}

#[tokio::test]
async fn nothing_is_sent_while_not_configured_or_disabled() {
    let h = Harness::new().await;
    let provider = FakeProvider::answering([]);
    let mut notifier = h.notifier(&provider);
    h.record(crash_loop("web")).await;
    pass(&mut notifier).await;
    h.enable(false).await;
    h.record(crash_loop("worker")).await;
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), 0);

    // Switched on, the events of the disabled period are skipped.
    h.enable(true).await;
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), 0);
    h.record(crash_loop("db")).await;
    pass(&mut notifier).await;
    assert_eq!(provider.subjects(), ["[srv-01] notes/db is crash looping"]);

    // Disabled and enabled again: the cursor moves to the newest event.
    h.enable(false).await;
    h.record(crash_loop("cache")).await;
    h.enable(true).await;
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), 1);
}

#[tokio::test]
async fn events_older_than_an_hour_are_not_mailed() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::answering([]);
    let mut notifier = h.notifier(&provider);
    h.store
        .record_event_at(
            &crash_loop("web"),
            Timestamp::now() - Triage::STALE - Duration::from_secs(60),
        )
        .await
        .unwrap();
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), 0);
    assert_eq!(
        h.store.notifier_cursor().await.unwrap(),
        h.newest_id().await
    );
}

#[tokio::test]
async fn the_hourly_cap_holds_back_a_burst() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::answering([]);
    let mut notifier = h.notifier(&provider);
    for i in 0..HourlyCap::LIMIT + 5 {
        h.record(crash_loop(&format!("web{i}"))).await;
    }
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), HourlyCap::LIMIT);
    assert_eq!(
        h.store.notified_problems().await.unwrap().len(),
        HourlyCap::LIMIT,
        "the held-back problems were not mailed"
    );
    assert_eq!(
        h.store.notifier_cursor().await.unwrap(),
        h.newest_id().await
    );
}

#[tokio::test]
async fn a_missing_api_key_is_a_delivery_failure() {
    let h = Harness::enabled().await;
    let secrets = SecretStore::new(h.store.clone(), &SecretKey::generate());
    let mut notifier = h.notifier_with(Arc::new(secrets));
    h.record(crash_loop("web")).await;
    pass(&mut notifier).await;
    let failures = h.failure_events().await;
    assert_eq!(failures.len(), 1);
    assert_eq!(
        failures[0].meta.as_deref(),
        Some("cannot set up the Resend client: no API key is stored")
    );
}

#[tokio::test]
async fn a_recorded_event_wakes_the_running_notifier() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::answering([]);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(h.notifier(&provider).run(cancel.clone()));
    // The timer is an hour: only the broadcast can wake it.
    h.record(crash_loop("web")).await;
    let sent = async {
        while provider.attempts() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), sent)
        .await
        .expect("woken by ui_events");
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("cancel stops the notifier")
        .unwrap();
}

#[tokio::test]
async fn shutdown_during_a_retry_wait_ends_promptly() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::failing(unavailable);
    let slow = Retry {
        first: Duration::from_secs(60),
        max: Duration::from_secs(60),
        window: Duration::from_secs(300),
    };
    let notifier = h
        .notifier(&provider)
        .with_timing(slow, Duration::from_secs(3600));
    h.record(crash_loop("web")).await;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(notifier.run(cancel.clone()));
    let tried = async {
        while provider.attempts() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), tried)
        .await
        .unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("the retry wait ends on cancel")
        .unwrap();
    // The mail was in flight: the cursor stays, so the next start tries it again.
    assert!(h.store.notifier_cursor().await.unwrap() < h.newest_id().await);
    assert!(h.failure_events().await.is_empty());
}

#[tokio::test]
async fn an_open_problem_is_reminded_of_without_a_repeat() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::answering([]);
    let mut notifier = h.notifier(&provider);
    h.record(crash_loop("web")).await;
    pass(&mut notifier).await;
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), 1, "not due yet");

    // The last mail went out a day ago, as a new notifier reads it from the store.
    let problem = h.store.notified_problems().await.unwrap().remove(0);
    let day_ago = Timestamp::now() - Triage::REMINDER;
    let step = Triage::reminded(&problem, day_ago);
    h.store.advance_notifier(&step).await.unwrap();
    let mut notifier = h.notifier(&provider);
    pass(&mut notifier).await;
    assert_eq!(
        provider.subjects()[1..],
        ["[srv-01] notes/web is crash looping"]
    );
    let message = provider.sent.lock().unwrap()[1].clone();
    assert!(
        message
            .idempotency_key
            .as_ref()
            .starts_with("hoster-reminder-")
    );
    assert!(message.text.contains("still failing"));
    let reminded = h.store.notified_problems().await.unwrap();
    assert_eq!(reminded[0].event, problem.event);
    assert!(reminded[0].at > day_ago);
    pass(&mut notifier).await;
    assert_eq!(provider.attempts(), 2, "the next one is a day later");
}

/// Switches notifications off when asked for the provider: a settings change made while a
/// pass is mailing.
struct DisablingMailer {
    provider: Arc<FakeProvider>,
    store: Store,
}

impl Mailer for DisablingMailer {
    fn provider(
        &self,
        _kind: ProviderKind,
    ) -> BoxFuture<'_, Result<Arc<dyn Provider>, DeliveryError>> {
        async move {
            let mut settings = self.store.notification_settings().await.unwrap().unwrap();
            settings.enabled = false;
            self.store
                .set_notification_settings(&settings)
                .await
                .unwrap();
            let provider: Arc<dyn Provider> = self.provider.clone();
            Ok(provider)
        }
        .boxed()
    }
}

#[tokio::test]
async fn a_settings_change_during_a_pass_applies_before_the_next_mail() {
    let h = Harness::enabled().await;
    let provider = FakeProvider::answering([]);
    let mut notifier = h.notifier_with(Arc::new(DisablingMailer {
        provider: provider.clone(),
        store: h.store.clone(),
    }));
    h.record(crash_loop("web")).await;
    h.record(crash_loop("worker")).await;
    pass(&mut notifier).await;
    assert_eq!(provider.subjects(), ["[srv-01] notes/web is crash looping"]);
}
