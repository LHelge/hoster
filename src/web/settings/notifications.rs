//! Settings → Notifications (ADR-0058 §5): the mail provider, its API key, the from address,
//! the one recipient, "enabled" and "also notify on successes". The settings are a row in
//! SQLite and the key is in the secret store (ADR-0029), write-only like a registry token: the
//! page says whether one is stored and when it last changed, and an empty key field on save
//! keeps it. The notifier reads both again on every pass, so a save applies within a minute
//! without a restart.
//!
//! **Send test** delivers the fixed test mail through the saved configuration and answers with
//! what the provider said, in plain words. It is refused before any request when nothing is
//! saved, and throttled ([`Notifications::COOLDOWN`]) so the button cannot use up the provider's
//! quota. Its result is the last delivery, like any notification's, which the status block
//! shows beside a link to hoster's warnings on the Events page, where delivery failures are
//! filed.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use askama::{DynTemplate, Template};
use axum::Extension;
use axum::extract::Form;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use reqwest::Url;
use serde::Deserialize;

use super::{Settings, SettingsNav};
use crate::instance::Instance;
use crate::notify::{Courier, DeliveryError, MailAddress, Mailer, ProviderKind, Tested};
use crate::orchestrator::{EventLog, NotificationEvents, Operator};
use crate::secrets::{SecretStore, SecretValue};
use crate::store::{
    DeliveryOutcome, LastDelivery, NotificationSettings, Store, StoreError, Timestamp,
};
use crate::web::Tone;
use crate::web::auth::Session;
use crate::web::pages::{Layout, Page};
use crate::web::stacks::{Outcome, When};
use crate::web::ui::Pill;

/// The section's handlers and what they need: the settings row, the key in the secret store,
/// the event log the key's changes are recorded in, and the notifier's [`Courier`] that sends
/// the test mail. Cheap to clone.
#[derive(Clone)]
pub struct Notifications(Arc<Inner>);

struct Inner {
    store: Store,
    secrets: SecretStore,
    events: EventLog,
    courier: Courier,
    /// When the last test mail was attempted, for the cooldown.
    last_test: Mutex<Option<Instant>>,
}

/// What is stored: the settings row, when the provider's key last changed, the last delivery.
struct Stored {
    settings: Option<NotificationSettings>,
    key_changed: Option<Timestamp>,
    last: Option<LastDelivery>,
}

/// A store or secret-store failure, logged; the page says only that it failed.
struct Unreadable;

impl From<Unreadable> for Outcome {
    fn from(_: Unreadable) -> Self {
        Outcome::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The notification settings could not be read or stored; see hoster's log.",
        )
    }
}

impl Notifications {
    /// The section's URL.
    pub const URL: &str = "/settings/notifications";
    /// The Events page showing hoster's own events of severity warning and above, where
    /// `NotificationFailed` is filed (ADR-0055 §2) beside the other operator warnings.
    const FAILURES: &str = "/activity?kind=operator&severity=warning";
    /// The shortest time between two test mails.
    pub const COOLDOWN: Duration = Duration::from_secs(10);

    /// The section, delivering test mail through `mailer` under `instance`'s name, with links
    /// to `admin`.
    pub fn new(
        store: Store,
        secrets: SecretStore,
        mailer: Arc<dyn Mailer>,
        events: EventLog,
        instance: Instance,
        admin: Url,
    ) -> Self {
        Self(Arc::new(Inner {
            courier: Courier::new(store.clone(), mailer, instance, admin),
            store,
            secrets,
            events,
            last_test: Mutex::new(None),
        }))
    }

    /// `GET /settings/notifications`, with the sub-nav `nav`.
    pub(super) async fn page(&self, layout: Layout, nav: SettingsNav) -> Response {
        match self.stored().await {
            Ok(stored) => Page(NotificationsPage {
                layout,
                nav,
                removable: stored.settings.is_some() || stored.key_changed.is_some(),
                status: StatusBlock::new(&stored, false),
                form: SettingsForm::new(&stored, false),
            })
            .into_response(),
            Err(err) => Outcome::from(err).page(layout),
        }
    }

    /// `POST /settings/notifications`: validates the form and stores the key (when one was
    /// entered) and the settings. The answer is the form again, with field errors or the
    /// stored values, and the status block out of band.
    pub async fn save(
        Extension(settings): Extension<Settings>,
        Extension(session): Extension<Session>,
        Form(form): Form<NotificationForm>,
    ) -> Response {
        let by = Operator::from(session.login());
        settings.notifications.store_form(form, &by).await
    }

    /// `POST /settings/notifications/test`: sends the test mail; the result goes in the test
    /// card, the status block out of band.
    pub async fn test(Extension(settings): Extension<Settings>) -> Response {
        settings.notifications.send_test().await
    }

    /// `POST /settings/notifications/remove`: deletes the settings and the API key.
    pub async fn remove(
        Extension(settings): Extension<Settings>,
        Extension(session): Extension<Session>,
    ) -> Response {
        let by = Operator::from(session.login());
        settings.notifications.remove_all(&by).await
    }

    async fn stored(&self) -> Result<Stored, Unreadable> {
        let settings = self
            .0
            .store
            .notification_settings()
            .await
            .map_err(|err| Self::log(&err))?;
        let provider = settings
            .as_ref()
            .map(|settings| settings.provider)
            .unwrap_or_default();
        let key_changed = self.key_changed(provider).await?;
        let last = self
            .0
            .store
            .last_delivery()
            .await
            .map_err(|err| Self::log(&err))?;
        Ok(Stored {
            settings,
            key_changed,
            last,
        })
    }

    /// When the API key of `provider` last changed; `None` when none is stored.
    async fn key_changed(&self, provider: ProviderKind) -> Result<Option<Timestamp>, Unreadable> {
        self.0
            .secrets
            .changed_at(provider.secret_name().as_ref())
            .await
            .map_err(|err| Self::log(&err))
    }

    fn log(err: &dyn std::error::Error) -> Unreadable {
        tracing::error!(error = %err, "reading or storing the notification settings failed");
        Unreadable
    }

    async fn store_form(&self, form: NotificationForm, by: &Operator) -> Response {
        let mut errors = FieldErrors::default();
        let provider = form
            .provider
            .trim()
            .parse::<ProviderKind>()
            .unwrap_or_else(|err| {
                errors.provider = Some(Outcome::sentence(&err.to_string()));
                ProviderKind::default()
            });
        let from = FieldErrors::address(&form.from, &mut errors.from);
        let recipient = FieldErrors::address(&form.recipient, &mut errors.recipient);
        let key = form
            .api_key
            .map(|key| key.expose().trim().to_owned())
            .filter(|key| !key.is_empty());
        if key
            .as_deref()
            .is_some_and(|key| !key.chars().all(|c| c.is_ascii_graphic()))
        {
            errors.api_key =
                Some("The API key is one line of printable characters, without spaces.".to_owned());
        }
        let enabled = form.enabled.is_some();
        let also_successes = form.also_successes.is_some();
        // The form as submitted, for an answer that keeps what the operator typed.
        let submitted = |key_changed: Option<Timestamp>, errors: FieldErrors| SettingsForm {
            provider,
            from: form.from.trim().to_owned(),
            recipient: form.recipient.trim().to_owned(),
            enabled,
            also_successes,
            key_changed: key_changed.map(|at| When(at).to_string()),
            errors,
            result: None,
            oob: false,
        };
        let key_changed = match self.key_changed(provider).await {
            Ok(changed) => changed,
            Err(err) => {
                return self
                    .refused(submitted(None, errors), Outcome::from(err))
                    .await;
            }
        };
        if enabled {
            let missing = |field: &mut Option<String>, what: &str| {
                field.get_or_insert_with(|| format!("Enter {what} to switch notifications on."));
            };
            if from.is_none() {
                missing(&mut errors.from, "the from address");
            }
            if recipient.is_none() {
                missing(&mut errors.recipient, "the recipient");
            }
            if key.is_none() && key_changed.is_none() {
                missing(&mut errors.api_key, "the API key (none is stored)");
            }
        }
        if errors.any() {
            let view = SettingsForm {
                result: Some(Outcome::error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Nothing was saved: correct the fields marked above.",
                )),
                ..submitted(key_changed, errors)
            };
            return Self::html(
                StatusCode::UNPROCESSABLE_ENTITY,
                [&view as &dyn DynTemplate],
            );
        }

        // The settings first, the key last: a failure leaves either nothing changed or the
        // settings saved without the new key, and the answer says which.
        let settings = NotificationSettings {
            provider,
            from,
            recipient,
            enabled,
            also_successes,
        };
        let unsaved = |status: StatusCode, message: &str| {
            (
                submitted(key_changed, FieldErrors::default()),
                Outcome::error(status, message.to_owned()),
            )
        };
        match self.0.store.set_notification_settings(&settings).await {
            Ok(()) => {}
            Err(StoreError::NotificationsIncomplete) => {
                let (view, outcome) = unsaved(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Enter the from address and the recipient to switch notifications on.",
                );
                return self.refused(view, outcome).await;
            }
            Err(err) => {
                let view = submitted(key_changed, FieldErrors::default());
                return self.refused(view, Outcome::from(Self::log(&err))).await;
            }
        }
        tracing::info!(enabled, "notification settings stored");
        if let Some(key) = &key {
            let event =
                NotificationEvents::key_set(provider.label(), key_changed.is_some(), Some(by));
            let value = SecretValue::from(key.as_str());
            match self
                .0
                .secrets
                .set_recorded(&provider.secret_name(), &value, &event)
                .await
            {
                Ok((_, event)) => self.0.events.announce(event),
                Err(err) => {
                    Self::log(&err);
                    let (view, outcome) = unsaved(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "The settings were saved, but the API key could not be stored; see \
                         hoster's log. Enter the key again and save.",
                    );
                    return self.refused(view, outcome).await;
                }
            }
            tracing::info!(provider = %provider, "notification API key stored");
        }

        let mut message = match settings.active() {
            Some((_, to)) if settings.also_successes => format!(
                "Saved. Notifications are on: problems, recoveries and successes are mailed to \
                 {to} from the next event on."
            ),
            Some((_, to)) => format!(
                "Saved. Notifications are on: problems and recoveries are mailed to {to} from \
                 the next event on."
            ),
            None => "Saved. Notifications are off: no mail is sent.".to_owned(),
        };
        if key.is_some() {
            message.push_str(" The API key is stored and is never shown again.");
        }
        let stored = match self.stored().await {
            Ok(stored) => stored,
            Err(err) => {
                let view = submitted(key_changed, FieldErrors::default());
                return self.refused(view, Outcome::from(err)).await;
            }
        };
        let form = SettingsForm {
            result: Some(Outcome::success(message)),
            ..SettingsForm::new(&stored, false)
        };
        let status = StatusBlock::new(&stored, true);
        Self::html(StatusCode::OK, [&form as &dyn DynTemplate, &status])
    }

    /// The answer to a save that failed in the store: `view`, the form as submitted, with
    /// `outcome` in its result area, so nothing typed is lost, and the status block out of
    /// band as it now is, when it can be read.
    async fn refused(&self, view: SettingsForm, outcome: Outcome) -> Response {
        let status = outcome.status();
        let view = SettingsForm {
            result: Some(outcome),
            ..view
        };
        let block = self
            .stored()
            .await
            .ok()
            .map(|stored| StatusBlock::new(&stored, true));
        let mut parts: Vec<&dyn DynTemplate> = vec![&view];
        if let Some(block) = &block {
            parts.push(block);
        }
        Self::html(status, parts)
    }

    async fn send_test(&self) -> Response {
        let stored = match self.stored().await {
            Ok(stored) => stored,
            Err(err) => return Outcome::from(err).into_response(),
        };
        let refuse = |message: &str| {
            Outcome::error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("{message} No mail was sent."),
            )
            .into_response()
        };
        let Some(settings) = &stored.settings else {
            return refuse(
                "Nothing is saved yet: save the API key, the from address and the recipient first.",
            );
        };
        let (Some(from), Some(to)) = (&settings.from, &settings.recipient) else {
            return refuse("Save the from address and the recipient first.");
        };
        if stored.key_changed.is_none() {
            return refuse(&format!(
                "No {} API key is stored: save one first.",
                settings.provider.label()
            ));
        }
        if let Some(wait) = self.cooldown() {
            return Outcome::error(
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Wait {} s before sending another test: the button is limited so that it \
                     cannot use up the provider's quota.",
                    wait.as_secs().max(1)
                ),
            )
            .into_response();
        }

        let Tested { result, last } = self.0.courier.send_test(settings.provider, from, to).await;
        let (status, outcome) = match result {
            Ok(delivery) => {
                let id = delivery
                    .id
                    .as_deref()
                    .map(|id| format!(" (id {id})"))
                    .unwrap_or_default();
                let outcome = Outcome::success(format!(
                    "Sent: {} accepted the test mail to {to}{id}. If it does not arrive, look in \
                     the spam folder.",
                    settings.provider.label()
                ));
                (StatusCode::OK, outcome)
            }
            Err(err) => (
                StatusCode::BAD_GATEWAY,
                Outcome::error(StatusCode::BAD_GATEWAY, Explained(&err).to_string()),
            ),
        };
        let stored = Stored {
            last: Some(last),
            ..stored
        };
        Self::html(
            status,
            [
                &outcome as &dyn DynTemplate,
                &StatusBlock::new(&stored, true),
            ],
        )
    }

    /// `None` when a test may be sent now (and the cooldown starts), else how long to wait.
    fn cooldown(&self) -> Option<Duration> {
        let mut last = self
            .0
            .last_test
            .lock()
            .expect("the cooldown lock is not poisoned");
        let now = Instant::now();
        if let Some(wait) = last
            .map(|at| Self::COOLDOWN.saturating_sub(now.duration_since(at)))
            .filter(|wait| !wait.is_zero())
        {
            return Some(wait);
        }
        *last = Some(now);
        None
    }

    async fn remove_all(&self, by: &Operator) -> Response {
        let stored = match self.stored().await {
            Ok(stored) => stored,
            Err(err) => return Outcome::from(err).into_response(),
        };
        // The keys first, the settings last: a failure leaves the settings in place, which
        // the form still shows, and the answer says what is gone.
        let mut keys_removed = false;
        for provider in ProviderKind::ALL {
            let event = NotificationEvents::key_deleted(provider.label(), Some(by));
            match self
                .0
                .secrets
                .remove_recorded(&provider.secret_name(), &event)
                .await
            {
                Ok(Some(event)) => {
                    keys_removed = true;
                    self.0.events.announce(event);
                }
                Ok(None) => {}
                Err(err) => {
                    Self::log(&err);
                    return self.remove_failed(keys_removed).await;
                }
            }
        }
        if let Err(err) = self.0.store.clear_notification_settings().await {
            Self::log(&err);
            return self.remove_failed(keys_removed).await;
        }
        let removed = keys_removed || stored.settings.is_some();
        if !removed {
            return Outcome::error(
                StatusCode::NOT_FOUND,
                "Nothing is configured, so there is nothing to remove.",
            )
            .into_response();
        }
        tracing::info!("notification settings and API key removed");
        let stored = Stored {
            settings: None,
            key_changed: None,
            last: stored.last,
        };
        let outcome = Outcome::success(
            "Removed the notification settings and the API key: no mail is sent until \
             notifications are configured again.",
        )
        .with_link(Self::URL.to_owned(), "Reload the page");
        Self::html(
            StatusCode::OK,
            [
                &outcome as &dyn DynTemplate,
                &StatusBlock::new(&stored, true),
                &SettingsForm::new(&stored, true),
            ],
        )
    }

    /// The answer to a removal that failed in the store, after the API key was removed when
    /// `keys_removed`: what is left, with the status block and the form out of band as they
    /// now are, when they can be read.
    async fn remove_failed(&self, keys_removed: bool) -> Response {
        let outcome = Outcome::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            if keys_removed {
                "The API key was removed, but the notification settings could not be; see \
                 hoster's log. Notifications fail without a key until they are removed."
            } else {
                "Nothing was removed: the notification settings could not be read or stored; \
                 see hoster's log."
            },
        );
        let Ok(stored) = self.stored().await else {
            return outcome.into_response();
        };
        Self::html(
            StatusCode::INTERNAL_SERVER_ERROR,
            [
                &outcome as &dyn DynTemplate,
                &StatusBlock::new(&stored, true),
                &SettingsForm::new(&stored, true),
            ],
        )
    }

    /// `parts` rendered one after the other, answered with `status`.
    fn html<'a>(
        status: StatusCode,
        parts: impl IntoIterator<Item = &'a dyn DynTemplate>,
    ) -> Response {
        let mut html = String::new();
        for part in parts {
            match part.dyn_render() {
                Ok(rendered) => html.push_str(&rendered),
                Err(err) => {
                    tracing::error!(error = %err, "rendering a template failed");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
        (status, Html(html)).into_response()
    }
}

/// The settings form. The key goes straight into a [`SecretValue`]; the checkboxes are present
/// only when ticked.
#[derive(Deserialize)]
pub struct NotificationForm {
    #[serde(default)]
    provider: String,
    api_key: Option<SecretValue>,
    #[serde(default)]
    from: String,
    #[serde(default)]
    recipient: String,
    enabled: Option<String>,
    also_successes: Option<String>,
}

/// Why a field of the form was refused, shown under it.
#[derive(Debug, Default)]
struct FieldErrors {
    provider: Option<String>,
    api_key: Option<String>,
    from: Option<String>,
    recipient: Option<String>,
}

impl FieldErrors {
    fn any(&self) -> bool {
        self.provider.is_some()
            || self.api_key.is_some()
            || self.from.is_some()
            || self.recipient.is_some()
    }

    /// The address in `raw`: `None` when empty, or when it is invalid, with the reason in
    /// `error`.
    fn address(raw: &str, error: &mut Option<String>) -> Option<MailAddress> {
        if raw.trim().is_empty() {
            return None;
        }
        raw.parse()
            .map_err(|err: crate::notify::InvalidMailAddress| {
                *error = Some(Outcome::sentence(&err.to_string()));
            })
            .ok()
    }
}

/// A provider in the form's select.
struct ProviderOption {
    name: &'static str,
    label: &'static str,
    selected: bool,
}

/// The settings card with its form. Rendered in the page, as the answer to a save (with its
/// result) and out of band after a removal.
#[derive(Template)]
#[template(path = "notification_form.html")]
struct SettingsForm {
    oob: bool,
    provider: ProviderKind,
    from: String,
    recipient: String,
    enabled: bool,
    also_successes: bool,
    /// When the stored key last changed; `None` when no key is stored.
    key_changed: Option<String>,
    errors: FieldErrors,
    /// The answer to a save, in the form's result area.
    result: Option<Outcome>,
}

impl SettingsForm {
    /// The form showing what is stored.
    fn new(stored: &Stored, oob: bool) -> Self {
        let settings = stored.settings.clone().unwrap_or_default();
        let text =
            |address: Option<MailAddress>| address.map(|a| a.to_string()).unwrap_or_default();
        Self {
            oob,
            provider: settings.provider,
            from: text(settings.from),
            recipient: text(settings.recipient),
            enabled: settings.enabled,
            also_successes: settings.also_successes,
            key_changed: stored.key_changed.map(|at| When(at).to_string()),
            errors: FieldErrors::default(),
            result: None,
        }
    }

    fn providers(&self) -> Vec<ProviderOption> {
        ProviderKind::ALL
            .into_iter()
            .map(|kind| ProviderOption {
                name: kind.name(),
                label: kind.label(),
                selected: kind == self.provider,
            })
            .collect()
    }
}

/// The status block: whether notifications are on, and the last delivery.
#[derive(Template)]
#[template(path = "notification_status.html")]
struct StatusBlock {
    oob: bool,
    state: Pill,
    detail: String,
    last: Option<LastView>,
    failures: &'static str,
}

/// The last delivery as the status block shows it.
struct LastView {
    at: String,
    result: Pill,
    detail: String,
}

impl StatusBlock {
    fn new(stored: &Stored, oob: bool) -> Self {
        let (state, detail) = match &stored.settings {
            None => (
                Tone::Neutral.pill("not configured"),
                "No mail is sent.".to_owned(),
            ),
            Some(settings) => match settings.active() {
                Some((from, to)) => (
                    Tone::Ok.pill("on"),
                    format!(
                        "Mailing {to} from {from} through {}{}.",
                        settings.provider.label(),
                        if settings.also_successes {
                            ", successes too"
                        } else {
                            ""
                        }
                    ),
                ),
                None => (
                    Tone::Neutral.pill("off"),
                    "Saved, but switched off: no mail is sent.".to_owned(),
                ),
            },
        };
        let last = stored.last.as_ref().map(|last| {
            let (result, detail) = match &last.outcome {
                DeliveryOutcome::Sent { id: Some(id) } => {
                    (Tone::Ok.pill("sent"), format!("accepted, id {id}"))
                }
                DeliveryOutcome::Sent { id: None } => {
                    (Tone::Ok.pill("sent"), "accepted".to_owned())
                }
                DeliveryOutcome::Failed { error } => (Tone::Fail.pill("failed"), error.clone()),
            };
            LastView {
                at: When(last.at).to_string(),
                result,
                detail,
            }
        });
        Self {
            oob,
            state,
            detail,
            last,
            failures: Notifications::FAILURES,
        }
    }
}

/// `GET /settings/notifications`.
#[derive(Template)]
#[template(path = "notifications.html")]
pub struct NotificationsPage {
    layout: Layout,
    nav: SettingsNav,
    status: StatusBlock,
    form: SettingsForm,
    /// Whether anything is stored that "Remove" would delete.
    removable: bool,
}

/// A delivery error for the test result: what went wrong in plain words
/// ([`DeliveryError::plain`]), then the provider's own answer, which never holds the key.
pub(super) struct Explained<'a>(pub(super) &'a DeliveryError);

impl fmt::Display for Explained<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}",
            self.0.plain(),
            Outcome::sentence(self.0.to_string().trim_end_matches('.'))
        )
    }
}
