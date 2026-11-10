//! Outbound mail for notifications (ADR-0058 §3, §4).
//!
//! The public surface is small: a [`Message`] to deliver, the [`Provider`] trait that delivers
//! it, the [`ProviderKind`] stored with the notification settings that builds a provider from
//! its API key, and the [`DeliveryError`] that tells the notifier whether to retry. The
//! providers themselves (Resend first) are private. Adding one means a new [`ProviderKind`]
//! variant and a new type implementing [`Provider`], with no change to callers.
//!
//! The trait is for mail only; SMS and chat are not designed for.
//!
//! What a notification says is rendered by [`Mail`] (`mail.rs`, ADR-0058 §10): a [`Notice`]
//! under a [`Letterhead`] gives the subject, plain text and HTML of a [`Message`].
//!
//! The [`Notifier`] (`notifier.rs`, ADR-0058 §6–§9) is the background task that reads the event
//! log and sends those mails.

mod mail;
mod notifier;
mod resend;

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::{StatusCode, Url};
use thiserror::Error;

use crate::secrets::{SecretName, SecretValue};

/// Sample events for the mail's tests and the UI preview.
#[cfg(test)]
pub(crate) use self::mail::tests as mail_fixtures;
pub use self::mail::{Letterhead, Mail, Notice};
pub use self::notifier::{Mailer, Notifier};

/// Longest a single delivery request may take. The notifier retries a timeout (ADR-0058 §9),
/// so this is short.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The `User-Agent` of every provider request. Resend answers 403 without one.
const USER_AGENT: &str = concat!("hoster/", env!("CARGO_PKG_VERSION"));

/// The longest mail address accepted (RFC 5321's path limit, less the angle brackets).
const MAX_ADDRESS_LEN: usize = 254;

/// The longest idempotency key accepted. Resend allows 256 characters.
const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;

/// A plain mail address, `local@domain`, checked enough that a provider will not reject its
/// shape and that it cannot inject a second recipient or a header.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MailAddress(String);

/// Why a mail address was rejected. The messages are shown in the UI.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidMailAddress {
    #[error("the address is empty")]
    Empty,
    #[error("the address is longer than {MAX_ADDRESS_LEN} characters")]
    TooLong,
    #[error("the address needs exactly one `@` between a name and a domain")]
    Shape,
    #[error("the address contains a space, a control character or one of `<>,;:\"()[]\\`")]
    Character,
    #[error("the domain `{0}` is not a valid domain name")]
    Domain(String),
}

impl FromStr for MailAddress {
    type Err = InvalidMailAddress;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(InvalidMailAddress::Empty);
        }
        if s.chars().count() > MAX_ADDRESS_LEN {
            return Err(InvalidMailAddress::TooLong);
        }
        if s.chars()
            .any(|c| c.is_whitespace() || c.is_control() || "<>,;:\"()[]\\".contains(c))
        {
            return Err(InvalidMailAddress::Character);
        }
        let (local, domain) = s.split_once('@').ok_or(InvalidMailAddress::Shape)?;
        if local.is_empty() || domain.is_empty() || domain.contains('@') {
            return Err(InvalidMailAddress::Shape);
        }
        let labels_ok = domain
            .split('.')
            .all(|l| !l.is_empty() && !l.starts_with('-') && !l.ends_with('-'));
        if !labels_ok || !domain.contains('.') {
            return Err(InvalidMailAddress::Domain(domain.to_owned()));
        }
        Ok(Self(s.to_owned()))
    }
}

impl fmt::Display for MailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for MailAddress {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The key a provider uses to drop a repeated request, so a retry after a timeout cannot send
/// the same mail twice (ADR-0058 §4). Derived from the event by the caller. Printable ASCII
/// only, so it is always a valid header value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

/// Why an idempotency key was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidIdempotencyKey {
    #[error("the idempotency key is empty")]
    Empty,
    #[error("the idempotency key is longer than {MAX_IDEMPOTENCY_KEY_LEN} characters")]
    TooLong,
    #[error("the idempotency key may only hold printable ASCII without spaces")]
    Character,
}

impl FromStr for IdempotencyKey {
    type Err = InvalidIdempotencyKey;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(InvalidIdempotencyKey::Empty);
        }
        if s.len() > MAX_IDEMPOTENCY_KEY_LEN {
            return Err(InvalidIdempotencyKey::TooLong);
        }
        if !s.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(InvalidIdempotencyKey::Character);
        }
        Ok(Self(s.to_owned()))
    }
}

impl fmt::Display for IdempotencyKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for IdempotencyKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// One mail to deliver: always both an HTML and a plain-text part (ADR-0058 §10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub from: MailAddress,
    pub to: MailAddress,
    pub subject: String,
    pub text: String,
    pub html: String,
    pub idempotency_key: IdempotencyKey,
}

/// What a provider answered to an accepted mail.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Delivery {
    /// The provider's id for the mail, when it sent one.
    pub id: Option<String>,
}

/// Delivers mail through one provider's API. The future is boxed so a provider can sit behind
/// `Arc<dyn Provider>`, which is also what tests fake.
pub trait Provider: fmt::Debug + Send + Sync {
    /// Delivers `message`, or says why not and whether trying again may help.
    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<Delivery, DeliveryError>>;
}

/// Which mail API delivers notifications: the value stored in the notification settings, as
/// its lowercase name (`resend`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ProviderKind {
    #[default]
    Resend,
}

/// The stored name is not a known provider.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown mail provider `{0}`")]
pub struct UnknownProvider(String);

impl ProviderKind {
    /// Every provider, in the order the UI offers them.
    pub const ALL: [Self; 1] = [Self::Resend];

    /// The name stored in the settings.
    pub fn name(self) -> &'static str {
        match self {
            Self::Resend => "resend",
        }
    }

    /// The provider's name as people write it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Resend => "Resend",
        }
    }

    /// The secret-store name of this provider's API key: `_notify/<name>`, beside the registry
    /// credentials' `_registry/` (ADR-0029). Stack names cannot start with `_`, so no app
    /// secret can take it.
    pub fn secret_name(self) -> SecretName {
        format!("_notify/{}", self.name())
            .parse()
            .expect("a provider name is a valid secret name")
    }

    /// The provider's public API endpoint.
    fn default_base(self) -> Url {
        let base = match self {
            Self::Resend => resend::BASE_URL,
        };
        Url::parse(base).expect("a valid provider URL")
    }

    /// Builds this provider with `api_key`, talking to its public API.
    pub fn build(self, api_key: SecretValue) -> Result<Arc<dyn Provider>, DeliveryError> {
        self.build_at(api_key, self.default_base())
    }

    /// Builds this provider with `api_key`, talking to `base` instead of the public API (tests).
    pub fn build_at(
        self,
        api_key: SecretValue,
        base: Url,
    ) -> Result<Arc<dyn Provider>, DeliveryError> {
        self.build_with(api_key, base, REQUEST_TIMEOUT)
    }

    fn build_with(
        self,
        api_key: SecretValue,
        base: Url,
        timeout: Duration,
    ) -> Result<Arc<dyn Provider>, DeliveryError> {
        let http = self.http_client(timeout)?;
        Ok(match self {
            Self::Resend => Arc::new(resend::Resend::new(http, api_key, base, timeout)),
        })
    }

    /// The HTTP client every provider uses: rustls with ring (ADR-0037), a request timeout and
    /// hoster's `User-Agent`.
    fn http_client(self, timeout: Duration) -> Result<reqwest::Client, DeliveryError> {
        // reqwest is built without a bundled provider; make ring the process default unless
        // Pingora already did.
        let _ = rustls::crypto::ring::default_provider().install_default();
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()
            .map_err(|e| DeliveryError::Client {
                provider: self,
                reason: e.without_url().to_string(),
            })
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for ProviderKind {
    type Err = UnknownProvider;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name() == s)
            .ok_or_else(|| UnknownProvider(s.to_owned()))
    }
}

/// Why a mail was not delivered. [`is_retryable`](Self::is_retryable) tells the notifier
/// whether to try again (ADR-0058 §9); the message is shown in Settings and the event log and
/// never contains the API key.
#[derive(Debug, Error)]
pub enum DeliveryError {
    /// The request could not be built (the HTTP client, or an API key a header cannot carry).
    /// Nothing was sent. Permanent.
    #[error("cannot set up the {} client: {reason}", provider.label())]
    Client {
        provider: ProviderKind,
        reason: String,
    },
    /// The request never got an answer (connection refused, DNS, TLS, reset). Retryable.
    #[error("cannot reach {}: {reason}", provider.label())]
    Network {
        provider: ProviderKind,
        reason: String,
    },
    /// No answer within the request timeout. Retryable.
    #[error("{} did not answer within {}", provider.label(), humantime::format_duration(*timeout))]
    Timeout {
        provider: ProviderKind,
        timeout: Duration,
    },
    /// The provider answered with an error that may pass: 5xx, a per-second rate limit, a
    /// concurrent request with the same idempotency key. Retryable, after `retry_after` when
    /// the provider named it.
    #[error("{}", Answer { provider: *provider, status: *status, code: code.as_deref(), message: message.as_deref(), retry_after: *retry_after })]
    Unavailable {
        provider: ProviderKind,
        status: StatusCode,
        code: Option<String>,
        message: Option<String>,
        retry_after: Option<Duration>,
    },
    /// The provider refused the mail and will refuse it again: a rejected key, an unverified
    /// sender, a validation error, an exhausted quota. Permanent.
    #[error("{}", Answer { provider: *provider, status: *status, code: code.as_deref(), message: message.as_deref(), retry_after: None })]
    Rejected {
        provider: ProviderKind,
        status: StatusCode,
        code: Option<String>,
        message: Option<String>,
    },
}

impl DeliveryError {
    /// Whether the same request may succeed later.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Network { .. } | Self::Timeout { .. } | Self::Unavailable { .. }
        )
    }

    /// How long the provider asked us to wait before trying again.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Unavailable { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The HTTP status the provider answered, if it answered.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "shown by Settings → Notifications as it lands")
    )]
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            Self::Unavailable { status, .. } | Self::Rejected { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The provider's own error code (Resend: `validation_error`, `daily_quota_exceeded`, …).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "shown by Settings → Notifications as it lands")
    )]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Unavailable { code, .. } | Self::Rejected { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    /// The provider's explanation, for the UI.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "shown by Settings → Notifications as it lands")
    )]
    pub fn provider_message(&self) -> Option<&str> {
        match self {
            Self::Unavailable { message, .. } | Self::Rejected { message, .. } => {
                message.as_deref()
            }
            _ => None,
        }
    }

    /// A transport failure: a timeout or a network error, never including the request URL's
    /// query (there is none) or any header.
    fn transport(provider: ProviderKind, error: reqwest::Error, timeout: Duration) -> Self {
        if error.is_timeout() {
            return Self::Timeout { provider, timeout };
        }
        let error = error.without_url();
        let mut reason = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            reason.push_str(": ");
            reason.push_str(&cause.to_string());
            source = cause.source();
        }
        Self::Network { provider, reason }
    }
}

/// The readable form of a provider's error answer:
/// `Resend refused the mail (403 Forbidden, validation_error): The domain is not verified`.
struct Answer<'a> {
    provider: ProviderKind,
    status: StatusCode,
    code: Option<&'a str>,
    message: Option<&'a str>,
    retry_after: Option<Duration>,
}

impl fmt::Display for Answer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} answered {}", self.provider.label(), self.status)?;
        if let Some(code) = self.code {
            write!(f, " ({code})")?;
        }
        if let Some(message) = self.message {
            write!(f, ": {message}")?;
        }
        if let Some(retry_after) = self.retry_after {
            write!(
                f,
                "; retry after {}",
                humantime::format_duration(retry_after)
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
