//! The Resend provider (ADR-0058 §4): one `POST /emails` per mail.
//!
//! Resend's error answers carry a machine-readable `name` and a human `message` next to the
//! status code (`{"statusCode": 422, "name": "validation_error", "message": "…"}`). The error
//! reference (resend.com/docs/api-reference/errors, checked 2026-11-10) lists the names per
//! status but shows no body, so every field is optional and an answer that does not parse
//! (a proxy's error page) is classified by its status code alone.

use std::fmt;
use std::time::Duration;

use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};

use super::{Delivery, DeliveryError, Message, Provider, ProviderKind};
use crate::secrets::SecretValue;

/// Resend's public API.
pub(super) const BASE_URL: &str = "https://api.resend.com/";

const IDEMPOTENCY_HEADER: &str = "idempotency-key";

/// The longest provider message kept for the UI, in characters.
const MAX_MESSAGE_LEN: usize = 500;

pub(super) struct Resend {
    http: reqwest::Client,
    api_key: SecretValue,
    endpoint: Url,
    timeout: Duration,
}

impl fmt::Debug for Resend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resend")
            .field("api_key", &self.api_key)
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

/// The request body of `POST /emails`.
#[derive(Serialize)]
struct SendEmail<'a> {
    from: &'a str,
    to: [&'a str; 1],
    subject: &'a str,
    html: &'a str,
    text: &'a str,
}

impl<'a> From<&'a Message> for SendEmail<'a> {
    fn from(message: &'a Message) -> Self {
        Self {
            from: message.from.as_ref(),
            to: [message.to.as_ref()],
            subject: &message.subject,
            html: &message.html,
            text: &message.text,
        }
    }
}

/// The answer to an accepted mail.
#[derive(Deserialize)]
struct Sent {
    id: Option<String>,
}

/// An error answer. Every field is optional; see the module docs.
#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    name: Option<String>,
    message: Option<String>,
}

impl ErrorBody {
    fn parse(bytes: &[u8]) -> Self {
        let mut body: Self = serde_json::from_slice(bytes).unwrap_or_default();
        body.name = body.name.filter(|n| !n.trim().is_empty());
        body.message = body
            .message
            .map(|m| m.trim().chars().take(MAX_MESSAGE_LEN).collect::<String>())
            .filter(|m| !m.is_empty());
        body
    }

    fn is(&self, name: &str) -> bool {
        self.name.as_deref() == Some(name)
    }

    /// Whether the same request may succeed later. A 429 is a per-second rate limit unless
    /// it names an exhausted quota; a 409 for a concurrent request with the same key passes
    /// once that request finishes; 5xx and 408 are the provider's own trouble.
    fn is_retryable(&self, status: StatusCode) -> bool {
        match status {
            StatusCode::TOO_MANY_REQUESTS => {
                !(self.is("daily_quota_exceeded") || self.is("monthly_quota_exceeded"))
            }
            StatusCode::CONFLICT => self.is("concurrent_idempotent_requests"),
            StatusCode::REQUEST_TIMEOUT => true,
            status => status.is_server_error(),
        }
    }
}

impl Resend {
    pub(super) fn new(
        http: reqwest::Client,
        api_key: SecretValue,
        base: Url,
        timeout: Duration,
    ) -> Self {
        let mut base = base;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let endpoint = base.join("emails").expect("a valid relative URL");
        Self {
            http,
            api_key,
            endpoint,
            timeout,
        }
    }

    async fn deliver(&self, message: &Message) -> Result<Delivery, DeliveryError> {
        let provider = ProviderKind::Resend;
        let mut authorization = HeaderValue::try_from(format!("Bearer {}", self.api_key.expose()))
            .map_err(|_| DeliveryError::Client {
                provider,
                reason: "the API key contains characters a header cannot carry".to_owned(),
            })?;
        authorization.set_sensitive(true);
        let response = self
            .http
            .post(self.endpoint.clone())
            .header(header::AUTHORIZATION, authorization)
            .header(IDEMPOTENCY_HEADER, message.idempotency_key.as_ref())
            .json(&SendEmail::from(message))
            .send()
            .await
            .map_err(|e| DeliveryError::transport(provider, e, self.timeout))?;
        let status = response.status();
        let retry_after = Self::retry_after(response.headers());
        let bytes = response
            .bytes()
            .await
            .map_err(|e| DeliveryError::transport(provider, e, self.timeout))?;
        if status.is_success() {
            let id = serde_json::from_slice::<Sent>(&bytes)
                .ok()
                .and_then(|sent| sent.id);
            return Ok(Delivery { id });
        }
        let body = ErrorBody::parse(&bytes);
        Err(if body.is_retryable(status) {
            DeliveryError::Unavailable {
                provider,
                status,
                retry_after,
                code: body.name,
                message: body.message,
            }
        } else {
            DeliveryError::Rejected {
                provider,
                status,
                code: body.name,
                message: body.message,
            }
        })
    }

    /// `retry-after` in whole (or fractional) seconds, as Resend sends it.
    fn retry_after(headers: &HeaderMap) -> Option<Duration> {
        headers
            .get(header::RETRY_AFTER)?
            .to_str()
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|s| s.is_finite() && *s >= 0.0)
            .and_then(|s| Duration::try_from_secs_f64(s).ok())
    }
}

impl Provider for Resend {
    fn send<'a>(&'a self, message: &'a Message) -> BoxFuture<'a, Result<Delivery, DeliveryError>> {
        self.deliver(message).boxed()
    }
}
