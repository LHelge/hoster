//! The notifier (ADR-0058 §6–§9): the background task that turns the event log into mail.
//!
//! It reads the `events` rows past its persisted cursor, oldest first. The `ui_events`
//! broadcast wakes it and a one-minute timer is the fallback, so a lagging receiver loses
//! nothing: the table is the source, the broadcast only a nudge. [`Triage`] picks what mails
//! and holds repeats back, [`HourlyCap`] limits the mails to 20 an hour, and each mail is
//! delivered through the configured provider with a short [`Retry`]. After every event the
//! cursor and the de-duplication record move in one store transaction, so a crash loses
//! nothing and repeats at most the mail in flight, which its idempotency key keeps the provider
//! from sending twice.
//!
//! The settings are read before every mail and the API key on every delivery, so a change in
//! Settings applies without a restart. Switching notifications on moves the cursor to the newest event
//! (`Store::set_notification_settings`), so nothing from before is mailed.

mod triage;

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use reqwest::Url;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use self::triage::{FailureEvents, HourlyCap, Mailing, Retry, Triage};
use super::{
    Delivery, DeliveryError, IdempotencyKey, Letterhead, Mail, MailAddress, Message, Notice,
    Provider, ProviderKind,
};
use crate::instance::Instance;
use crate::orchestrator::{EventLog, NotificationEvents, UiEvent};
use crate::secrets::SecretStore;
use crate::store::{
    DeliveryOutcome, Event, LastDelivery, NotificationSettings, Store, StoreError, Timestamp,
};

/// Builds the provider the settings name, with its API key. [`SecretStore`] in production;
/// the tests hand out fakes.
pub trait Mailer: Send + Sync {
    /// The provider of `kind`, ready to send, or why it cannot be.
    fn provider(
        &self,
        kind: ProviderKind,
    ) -> BoxFuture<'_, Result<Arc<dyn Provider>, DeliveryError>>;
}

/// The API key is the secret [`ProviderKind::secret_name`] names.
impl Mailer for SecretStore {
    fn provider(
        &self,
        kind: ProviderKind,
    ) -> BoxFuture<'_, Result<Arc<dyn Provider>, DeliveryError>> {
        async move {
            let unusable = |reason: String| DeliveryError::Client {
                provider: kind,
                reason,
            };
            match self.reveal(kind.secret_name().as_ref()).await {
                Ok(Some((key, _))) => kind.build(key),
                Ok(None) => Err(unusable("no API key is stored".to_owned())),
                Err(err) => Err(unusable(format!("cannot read the API key: {err}"))),
            }
        }
        .boxed()
    }
}

/// How one delivery ended.
enum Outcome {
    Delivered,
    Dropped,
    /// hoster is shutting down; the mail may or may not have gone out.
    Cancelled,
}

/// What a delivery needs besides the decision to send: the letterhead a [`Notice`] is rendered
/// under, the provider it goes through and the store its result is recorded in as the last
/// delivery. The notifier and Settings' Send test both deliver through it, so a mail is
/// rendered, keyed and recorded the same way whichever sent it. Cheap to clone.
#[derive(Clone)]
pub struct Courier {
    store: Store,
    mailer: Arc<dyn Mailer>,
    instance: Instance,
    /// The admin origin the mails' buttons link to.
    admin: Url,
}

/// How a test mail went: its result, and the last delivery recorded for it.
pub struct Tested {
    pub result: Result<Delivery, DeliveryError>,
    pub last: LastDelivery,
}

impl Courier {
    /// A courier building providers with `mailer`, signing with `instance`'s name, linking to
    /// `admin` and recording the last delivery in `store`.
    pub fn new(store: Store, mailer: Arc<dyn Mailer>, instance: Instance, admin: Url) -> Self {
        Self {
            store,
            mailer,
            instance,
            admin,
        }
    }

    /// Sends the fixed test mail (ADR-0058 §5) from `from` to `to` through `kind`, once and
    /// without retrying, and records the result as the last delivery.
    pub async fn send_test(
        &self,
        kind: ProviderKind,
        from: &MailAddress,
        to: &MailAddress,
    ) -> Tested {
        let at = Timestamp::now();
        let route = Route {
            kind,
            from: from.clone(),
            to: to.clone(),
        };
        let result = match self.message(&route, Notice::Test { at }, Key::Test(at)) {
            Some(message) => match self.mailer.provider(kind).await {
                Ok(provider) => provider.send(&message).await,
                Err(err) => Err(err),
            },
            None => Err(DeliveryError::Client {
                provider: kind,
                reason: "the test mail could not be rendered".to_owned(),
            }),
        };
        let outcome = match &result {
            Ok(delivery) => {
                tracing::info!(id = delivery.id, "test notification sent");
                DeliveryOutcome::Sent {
                    id: delivery.id.clone(),
                }
            }
            Err(err) => {
                tracing::info!(error = %err, "test notification not delivered");
                DeliveryOutcome::Failed {
                    error: err.to_string(),
                }
            }
        };
        let last = self.record(outcome).await;
        Tested { result, last }
    }

    /// `notice` rendered under the letterhead as a message along `route`; `None`, logged, if
    /// it cannot be rendered.
    fn message(&self, route: &Route, notice: Notice<'_>, key: Key<'_>) -> Option<Message> {
        let letterhead = Letterhead::new(self.instance.name(), self.admin.clone());
        match Mail::try_from((&letterhead, notice)) {
            Ok(mail) => Some(mail.into_message(route.from.clone(), route.to.clone(), key.into())),
            Err(err) => {
                tracing::error!(error = %err, "cannot render a notification mail");
                None
            }
        }
    }

    /// Records `outcome`, now, as the last delivery; a store failure is logged.
    async fn record(&self, outcome: DeliveryOutcome) -> LastDelivery {
        let last = LastDelivery {
            at: Timestamp::now(),
            outcome,
        };
        if let Err(err) = self.store.record_delivery(&last).await {
            tracing::warn!(
                error = format!("{:#}", anyhow::Error::from(err)),
                "cannot record the last notification delivery"
            );
        }
        last
    }
}

/// The notifier task. Build it with [`Notifier::new`] and [`run`](Notifier::run) it.
pub struct Notifier {
    store: Store,
    events: EventLog,
    courier: Courier,
    /// `ui_events`, as a wake-up only; `None` once the channel closed.
    wake: Option<broadcast::Receiver<UiEvent>>,
    /// The fallback wake-up.
    tick: Duration,
    retry: Retry,
    cap: HourlyCap,
    failures: FailureEvents,
}

impl Notifier {
    /// How many events one store read takes.
    const BATCH: u32 = 100;
    /// How often the notifier looks without a wake-up, and sends a due summary.
    const TICK: Duration = Duration::from_secs(60);

    /// A notifier reading `store`, recording delivery failures in `events`, building providers
    /// with `mailer`, signing with `instance`'s name and linking to `admin`, woken by `wake`.
    pub fn new(
        store: Store,
        events: EventLog,
        mailer: Arc<dyn Mailer>,
        instance: Instance,
        admin: Url,
        wake: broadcast::Receiver<UiEvent>,
    ) -> Self {
        Self {
            courier: Courier::new(store.clone(), mailer, instance, admin),
            store,
            events,
            wake: Some(wake),
            tick: Self::TICK,
            retry: Retry::default(),
            cap: HourlyCap::default(),
            failures: FailureEvents::default(),
        }
    }

    /// Replaces the retry back-off and the fallback timer (tests).
    #[cfg(test)]
    fn with_timing(mut self, retry: Retry, tick: Duration) -> Self {
        self.retry = retry;
        self.tick = tick;
        self
    }

    /// Mails what the event log calls for until `cancel` fires. A pass that fails on the
    /// store is logged and tried again at the next wake-up.
    pub async fn run(mut self, cancel: CancellationToken) {
        loop {
            if let Err(err) = self.pass(&cancel).await {
                tracing::warn!(
                    error = format!("{:#}", anyhow::Error::from(err)),
                    "the notifier cannot read or store its state; trying again"
                );
            }
            tokio::select! {
                () = cancel.cancelled() => break,
                () = Self::woken(&mut self.wake) => {}
                () = tokio::time::sleep(self.tick) => {}
            }
        }
        tracing::debug!("notifier stopped");
    }

    /// Resolves at the next `ui_events` message, or never once the channel closed. Lagging
    /// is a wake-up too: the events are read from the store.
    async fn woken(wake: &mut Option<broadcast::Receiver<UiEvent>>) {
        let Some(rx) = wake else {
            return std::future::pending().await;
        };
        match rx.recv().await {
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => *wake = None,
        }
    }

    /// Handles every event past the cursor, then sends the reminders that are due, if
    /// notifications are on. The settings are read again before each mail: a change starts
    /// the pass over with the new settings, the cursor and the open problems the store has
    /// then, so switching notifications off, or off and on, applies at once.
    async fn pass(&mut self, cancel: &CancellationToken) -> Result<(), StoreError> {
        'settings: loop {
            let Some(settings) = self.store.notification_settings().await? else {
                return Ok(());
            };
            let Some((from, to)) = settings.active() else {
                return Ok(());
            };
            let route = Route {
                kind: settings.provider,
                from: from.clone(),
                to: to.clone(),
            };
            let mut triage = Triage::new(
                self.store.notified_problems().await?,
                settings.also_successes,
            );
            if !self.summarise(&route, cancel).await {
                return Ok(());
            }
            loop {
                let cursor = self.store.notifier_cursor().await?;
                let batch = self.store.events_after(cursor, Self::BATCH).await?;
                for event in &batch {
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                    if self.settings_changed(&settings).await? {
                        continue 'settings;
                    }
                    let verdict = triage.judge(event, Timestamp::now());
                    let delivered = match verdict.mail {
                        Some(mailing) => {
                            let notice = match mailing {
                                Mailing::Event => Notice::Event(event),
                                Mailing::Reminder => Notice::Reminder(event),
                            };
                            let key = Key::Event(event);
                            match self.mail(&route, notice, key, cancel).await {
                                Some(delivered) => delivered,
                                None => return Ok(()),
                            }
                        }
                        None => false,
                    };
                    let step = verdict.step(event, delivered, Timestamp::now());
                    self.store.advance_notifier(&step).await?;
                    triage.settle(&step);
                }
                if batch.len() < Self::BATCH as usize {
                    break;
                }
            }
            for problem in triage.reminders(Timestamp::now()) {
                if cancel.is_cancelled() {
                    return Ok(());
                }
                if self.settings_changed(&settings).await? {
                    continue 'settings;
                }
                // The event is gone once the retention sweep deleted it; the problem then
                // stays open without more reminders.
                if let Some(event) = self.store.event(problem.event).await? {
                    let now = Timestamp::now();
                    let notice = Notice::Reminder(&event);
                    let key = Key::Reminder(&event, now);
                    if self.mail(&route, notice, key, cancel).await.is_none() {
                        return Ok(());
                    }
                }
                let step = Triage::reminded(&problem, Timestamp::now());
                self.store.advance_notifier(&step).await?;
                triage.settle(&step);
            }
            return Ok(());
        }
    }

    /// Whether the stored settings are no longer `settings`.
    async fn settings_changed(&self, settings: &NotificationSettings) -> Result<bool, StoreError> {
        Ok(self.store.notification_settings().await?.as_ref() != Some(settings))
    }

    /// Mails `notice` within the hourly cap, after the cap's summary if one is due: whether it
    /// was delivered, or `None` if hoster is shutting down.
    async fn mail(
        &mut self,
        route: &Route,
        notice: Notice<'_>,
        key: Key<'_>,
        cancel: &CancellationToken,
    ) -> Option<bool> {
        if !self.summarise(route, cancel).await {
            return None;
        }
        if !self.cap.admit(Timestamp::now()) {
            tracing::info!("notification held back by the hourly cap");
            return Some(false);
        }
        match self.deliver(route, notice, key, cancel).await {
            Outcome::Delivered => Some(true),
            Outcome::Dropped => Some(false),
            Outcome::Cancelled => None,
        }
    }

    /// Sends the cap's summary if one is due. `false` if hoster is shutting down.
    async fn summarise(&mut self, route: &Route, cancel: &CancellationToken) -> bool {
        let now = Timestamp::now();
        let Some(more) = self.cap.summary(now) else {
            return true;
        };
        let notice = Notice::Summary { more, at: now };
        !matches!(
            self.deliver(route, notice, Key::Summary(now), cancel).await,
            Outcome::Cancelled
        )
    }

    /// Renders `notice`, delivers it along `route` and records the result as the last
    /// delivery; a dropped mail also writes a delivery-failed event, at most one an hour.
    async fn deliver(
        &mut self,
        route: &Route,
        notice: Notice<'_>,
        key: Key<'_>,
        cancel: &CancellationToken,
    ) -> Outcome {
        let Some(message) = self.courier.message(route, notice, key) else {
            return Outcome::Dropped;
        };
        let result = tokio::select! {
            () = cancel.cancelled() => return Outcome::Cancelled,
            result = self.send(route.kind, &message, cancel) => result,
        };
        let (outcome, delivery) = match result {
            Ok(Some(delivery)) => {
                tracing::info!(
                    id = delivery.id,
                    subject = message.subject,
                    "notification sent"
                );
                (
                    Outcome::Delivered,
                    DeliveryOutcome::Sent { id: delivery.id },
                )
            }
            Ok(None) => return Outcome::Cancelled,
            Err(err) => {
                let error = err.to_string();
                tracing::warn!(error, subject = message.subject, "notification dropped");
                if self.failures.due(Timestamp::now()) {
                    self.events.record(NotificationEvents::failed(&error)).await;
                }
                (Outcome::Dropped, DeliveryOutcome::Failed { error })
            }
        };
        self.courier.record(delivery).await;
        outcome
    }

    /// Sends `message` through the provider of `kind`, retrying a retryable error with
    /// back-off within the retry window. `Ok(None)` if `cancel` fired during a wait.
    async fn send(
        &self,
        kind: ProviderKind,
        message: &Message,
        cancel: &CancellationToken,
    ) -> Result<Option<Delivery>, DeliveryError> {
        let provider = self.courier.mailer.provider(kind).await?;
        let started = Instant::now();
        let mut failures = 0;
        loop {
            let err = match provider.send(message).await {
                Ok(delivery) => return Ok(Some(delivery)),
                Err(err) if !err.is_retryable() => return Err(err),
                Err(err) => err,
            };
            failures += 1;
            let Some(wait) = self
                .retry
                .wait(failures, err.retry_after(), started.elapsed())
            else {
                return Err(err);
            };
            tracing::info!(error = %err, ?wait, "notification delivery failed; retrying");
            tokio::select! {
                () = cancel.cancelled() => return Ok(None),
                () = tokio::time::sleep(wait) => {}
            }
        }
    }
}

/// Who a pass's mails go from and to, and through which provider.
struct Route {
    kind: ProviderKind,
    from: MailAddress,
    to: MailAddress,
}

/// What a mail's idempotency key is derived from. It holds the event's time as well as its
/// id, so a restored backup that hands out an id again within the provider's 24 hours does
/// not have its mail dropped as a repeat.
enum Key<'a> {
    Event(&'a Event),
    /// A reminder about the event, sent at the time.
    Reminder(&'a Event, Timestamp),
    Summary(Timestamp),
    /// Settings' Send test, pressed at the time.
    Test(Timestamp),
}

impl From<Key<'_>> for IdempotencyKey {
    fn from(key: Key<'_>) -> Self {
        let key = match key {
            Key::Event(event) => format!("hoster-event-{}-{}", event.id, event.at.as_millis()),
            Key::Reminder(event, at) => format!(
                "hoster-reminder-{}-{}-{}",
                event.id,
                event.at.as_millis(),
                at.as_millis()
            ),
            Key::Summary(at) => format!("hoster-summary-{}", at.as_millis()),
            Key::Test(at) => format!("hoster-test-{}", at.as_millis()),
        };
        key.parse().expect("digits and dashes are a valid key")
    }
}

#[cfg(test)]
mod tests;
