//! HTTP-01 issuance of one certificate for one hostname (ADR-0015, docs/architecture.md §7.3).
//!
//! [`Issuer::issue`] runs the whole order: `new_order` for a single DNS identifier (one
//! certificate per hostname, ADR-0035), the HTTP-01 challenge of every pending authorization
//! published in the [`ChallengeMap`] *before* the CA is told it is ready, `poll_ready`,
//! `finalize` (instant-acme generates the key and CSR), `poll_certificate`, and finally
//! [`CertMap::insert`], which validates, persists and publishes the certificate. The published
//! tokens are withdrawn when the order ends, however it ends.
//!
//! The CA is reached through the private [`Ca`]/[`CaOrder`] traits, implemented for instant-acme's
//! [`Account`] and [`Order`]; the unit tests drive the same flow against a scripted fake CA.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use instant_acme::{
    Account, AuthorizationStatus, CertificateIdentifier, ChallengeType, Identifier, NewOrder,
    Order, OrderStatus, Problem, RetryPolicy,
};

use super::certs::{CertError, CertMap, Validity};
use super::challenge::ChallengeMap;

/// Why a certificate could not be issued for a host.
#[derive(Debug, thiserror::Error)]
#[error("cannot issue a certificate for {host}")]
pub struct IssueError {
    pub host: String,
    #[source]
    pub failure: IssueFailure,
}

/// What went wrong in an issuance, so the caller can back off accordingly.
#[derive(Debug, thiserror::Error)]
pub enum IssueFailure {
    /// The CA could not validate the HTTP-01 challenge (DNS not pointing here, :80 unreachable,
    /// a wrong answer, CAA, ...). `detail` is the CA's explanation.
    #[error("the CA could not validate the HTTP-01 challenge: {detail}")]
    Validation { detail: String },
    /// The CA refused because of a rate limit (RFC 8555 `rateLimited`).
    #[error("rate limited by the CA: {detail}")]
    RateLimited { detail: String },
    /// The order did not become ready or valid within the polling budget.
    #[error("timed out waiting for the CA")]
    Timeout,
    /// None of the CA's challenges for the host is HTTP-01.
    #[error("the CA offered no HTTP-01 challenge")]
    NoHttp01,
    /// Any other ACME protocol or transport error.
    #[error("ACME request failed")]
    Acme(#[source] instant_acme::Error),
    /// The host is not a name the certificate store accepts (checked before ordering), or the
    /// issued certificate was rejected or could not be stored.
    #[error(transparent)]
    Store(CertError),
}

impl IssueFailure {
    /// RFC 8555 §6.7 problem types that mean the CA could not validate the challenge.
    const VALIDATION_PROBLEMS: [&str; 6] = [
        "unauthorized",
        "connection",
        "dns",
        "caa",
        "incorrectResponse",
        "tls",
    ];
    const ACME_ERROR_NS: &str = "urn:ietf:params:acme:error:";
}

impl From<instant_acme::Error> for IssueFailure {
    fn from(error: instant_acme::Error) -> Self {
        match error {
            instant_acme::Error::Api(problem) => {
                let kind = problem
                    .r#type
                    .as_deref()
                    .and_then(|t| t.strip_prefix(Self::ACME_ERROR_NS));
                match kind {
                    Some("rateLimited") => IssueFailure::RateLimited {
                        detail: CaDetail(&problem).to_string(),
                    },
                    Some(kind) if Self::VALIDATION_PROBLEMS.contains(&kind) => {
                        IssueFailure::Validation {
                            detail: CaDetail(&problem).to_string(),
                        }
                    }
                    _ => IssueFailure::Acme(instant_acme::Error::Api(problem)),
                }
            }
            instant_acme::Error::Timeout(_) => IssueFailure::Timeout,
            other => IssueFailure::Acme(other),
        }
    }
}

impl From<CertError> for IssueFailure {
    fn from(error: CertError) -> Self {
        IssueFailure::Store(error)
    }
}

/// A CA problem document as one line: its detail (or type) and those of its subproblems.
struct CaDetail<'a>(&'a Problem);

impl fmt::Display for CaDetail<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let problem = self.0;
        match (&problem.detail, &problem.r#type) {
            (Some(detail), _) => f.write_str(detail)?,
            (None, Some(kind)) => f.write_str(kind)?,
            (None, None) => f.write_str("no detail given")?,
        }
        for sub in &problem.subproblems {
            if let Some(detail) = sub.detail.as_deref().or(sub.r#type.as_deref()) {
                write!(f, "; {detail}")?;
            }
        }
        Ok(())
    }
}

/// An HTTP-01 challenge the CA wants answered on `:80`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Http01 {
    token: String,
    key_authorization: String,
}

/// HTTP-01 challenges published in the [`ChallengeMap`] for one order; withdrawn on drop, so a
/// failed or cancelled order leaves nothing behind.
struct Published {
    map: ChallengeMap,
    tokens: Vec<String>,
}

impl Published {
    /// Answers every challenge in `challenges` on `:80` until the order ends.
    fn new(map: ChallengeMap, challenges: &[Http01]) -> Self {
        for challenge in challenges {
            map.insert(
                challenge.token.as_str(),
                challenge.key_authorization.as_str(),
            );
        }
        let tokens = challenges.iter().map(|c| c.token.clone()).collect();
        Self { map, tokens }
    }
}

impl Drop for Published {
    fn drop(&mut self) {
        for token in &self.tokens {
            self.map.remove(token);
        }
    }
}

/// The window in which the CA suggests renewing a certificate (ACME Renewal Information,
/// RFC 9773).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenewalWindow {
    pub start: SystemTime,
    pub end: SystemTime,
}

/// The ACME CA, as far as issuance needs it.
#[async_trait]
trait Ca: Send + Sync {
    /// Opens an order for exactly one DNS identifier, `host`.
    async fn new_order(&self, host: &str) -> Result<Box<dyn CaOrder>, instant_acme::Error>;

    /// The CA's suggested renewal window for the certificate `id` and how long until it may be
    /// asked again (its `Retry-After`); `None` when the CA does not support ARI.
    async fn renewal_window(
        &self,
        _id: &CertificateIdentifier<'_>,
    ) -> Result<Option<(RenewalWindow, Duration)>, instant_acme::Error> {
        Ok(None)
    }
}

/// One open ACME order.
#[async_trait]
trait CaOrder: Send {
    /// The HTTP-01 challenge of every pending authorization. Already valid authorizations need
    /// none; any other authorization status fails the order.
    async fn http01_challenges(&mut self) -> Result<Vec<Http01>, IssueFailure>;

    /// Tells the CA that `challenge` is being answered and can be validated now.
    async fn set_ready(&mut self, challenge: &Http01) -> Result<(), instant_acme::Error>;

    /// Polls until the order is `Ready` or `Invalid`.
    async fn poll_ready(&mut self, retry: &RetryPolicy)
    -> Result<OrderStatus, instant_acme::Error>;

    /// Why an invalid order failed, as the CA recorded it on the order or a challenge.
    async fn validation_problem(&mut self) -> Option<Problem>;

    /// Generates a key and CSR for the order's identifiers and submits it; returns the key PEM.
    async fn finalize(&mut self) -> Result<String, instant_acme::Error>;

    /// Polls until the certificate is issued; returns the chain PEM.
    async fn poll_certificate(
        &mut self,
        retry: &RetryPolicy,
    ) -> Result<String, instant_acme::Error>;
}

#[async_trait]
impl Ca for Account {
    async fn new_order(&self, host: &str) -> Result<Box<dyn CaOrder>, instant_acme::Error> {
        let identifiers = [Identifier::Dns(host.to_owned())];
        let order = Account::new_order(self, &NewOrder::new(&identifiers)).await?;
        Ok(Box::new(order))
    }

    async fn renewal_window(
        &self,
        id: &CertificateIdentifier<'_>,
    ) -> Result<Option<(RenewalWindow, Duration)>, instant_acme::Error> {
        match self.renewal_info(id).await {
            Ok((info, retry_after)) => Ok(Some((
                RenewalWindow {
                    start: info.suggested_window.start.into(),
                    end: info.suggested_window.end.into(),
                },
                retry_after,
            ))),
            Err(instant_acme::Error::Unsupported(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl CaOrder for Order {
    async fn http01_challenges(&mut self) -> Result<Vec<Http01>, IssueFailure> {
        let mut challenges = Vec::new();
        let mut authorizations = self.authorizations();
        while let Some(authz) = authorizations.next().await {
            let mut authz = authz?;
            match authz.status {
                AuthorizationStatus::Valid => continue,
                AuthorizationStatus::Pending => {}
                status => {
                    let detail = match authz.challenges.iter().find_map(|c| c.error.as_ref()) {
                        Some(problem) => CaDetail(problem).to_string(),
                        None => format!("the authorization is {status:?}"),
                    };
                    return Err(IssueFailure::Validation { detail });
                }
            }
            let challenge = authz
                .challenge(ChallengeType::Http01)
                .ok_or(IssueFailure::NoHttp01)?;
            challenges.push(Http01 {
                token: challenge.token.clone(),
                key_authorization: challenge.key_authorization().as_str().to_owned(),
            });
        }
        Ok(challenges)
    }

    async fn set_ready(&mut self, challenge: &Http01) -> Result<(), instant_acme::Error> {
        // The authorizations were fetched by `http01_challenges`; walking them again is offline.
        let mut authorizations = self.authorizations();
        while let Some(authz) = authorizations.next().await {
            let mut authz = authz?;
            if let Some(mut handle) = authz.challenge(ChallengeType::Http01)
                && handle.token == challenge.token
            {
                return handle.set_ready().await;
            }
        }
        Err(instant_acme::Error::Str(
            "the HTTP-01 challenge is no longer part of the order",
        ))
    }

    async fn poll_ready(
        &mut self,
        retry: &RetryPolicy,
    ) -> Result<OrderStatus, instant_acme::Error> {
        Order::poll_ready(self, retry).await
    }

    async fn validation_problem(&mut self) -> Option<Problem> {
        if let Some(problem) = self.state().error.clone() {
            return Some(problem);
        }
        let mut authorizations = self.authorizations();
        while let Some(Ok(mut authz)) = authorizations.next().await {
            let Ok(state) = authz.refresh().await else {
                continue;
            };
            if let Some(problem) = state.challenges.iter().find_map(|c| c.error.clone()) {
                return Some(problem);
            }
        }
        None
    }

    async fn finalize(&mut self) -> Result<String, instant_acme::Error> {
        Order::finalize(self).await
    }

    async fn poll_certificate(
        &mut self,
        retry: &RetryPolicy,
    ) -> Result<String, instant_acme::Error> {
        Order::poll_certificate(self, retry).await
    }
}

/// Issues certificates over HTTP-01 and installs them in the [`CertMap`]. Cloning shares the
/// account and maps.
#[derive(Clone)]
pub struct Issuer {
    ca: std::sync::Arc<dyn Ca>,
    challenges: ChallengeMap,
    certs: CertMap,
    /// How long to wait for validation and for the certificate.
    retry: RetryPolicy,
    /// The CA's last renewal information per host, until its `Retry-After` has passed.
    ari: std::sync::Arc<Mutex<HashMap<String, AriAnswer>>>,
}

/// A renewal information answer, kept until the CA may be asked again (RFC 9773 §4.3).
#[derive(Debug, Clone)]
struct AriAnswer {
    /// The certificate it is about (its ARI identifier).
    cert: String,
    /// `None` when the CA does not support ARI.
    window: Option<RenewalWindow>,
    /// When the CA may be asked again.
    until: tokio::time::Instant,
}

impl fmt::Debug for Issuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Issuer")
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl Issuer {
    /// Polling for validation and issuance: 1 s, growing by half each time, for at most 3 min.
    const RETRY: RetryPolicy = RetryPolicy::new()
        .initial_delay(Duration::from_secs(1))
        .backoff(1.5)
        .timeout(Duration::from_secs(180));

    /// The bounds on how long a renewal information answer is kept, whatever the CA's
    /// `Retry-After` says. A CA without ARI is asked again after the maximum.
    const ARI_RETRY_MIN: Duration = Duration::from_secs(60);
    const ARI_RETRY_MAX: Duration = Duration::from_secs(24 * 60 * 60);

    /// An issuer ordering from `account`, answering challenges through `challenges` (the map the
    /// `:80` service reads) and installing certificates in `certs` (the one `:443` serves).
    pub fn new(account: Account, challenges: ChallengeMap, certs: CertMap) -> Self {
        Self::with_ca(std::sync::Arc::new(account), challenges, certs)
    }

    fn with_ca(ca: std::sync::Arc<dyn Ca>, challenges: ChallengeMap, certs: CertMap) -> Self {
        Self {
            ca,
            challenges,
            certs,
            retry: Self::RETRY,
            ari: std::sync::Arc::default(),
        }
    }

    /// Replaces the polling budget for validation and issuance.
    #[allow(dead_code, reason = "for integration tests against a test CA")]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Issues a certificate whose only name is `host` and installs it, replacing any previous one
    /// for that host. Returns its validity.
    pub async fn issue(&self, host: &str) -> Result<Validity, IssueError> {
        // A name the store would refuse is not worth an order (and a CA rate-limit slot).
        let host = match CertMap::normalize_host(host) {
            Ok(host) => host,
            Err(error) => {
                return Err(IssueError {
                    host: host.to_owned(),
                    failure: IssueFailure::Store(error),
                });
            }
        };
        tracing::info!(host, "requesting certificate");
        match self.run(&host).await {
            Ok(validity) => Ok(validity),
            Err(failure) => {
                tracing::warn!(host, error = %failure, "certificate issuance failed");
                Err(IssueError { host, failure })
            }
        }
    }

    /// When `host`'s current certificate is valid, if it has one.
    pub fn validity(&self, host: &str) -> Option<Validity> {
        self.certs.validity(host)
    }

    /// The CA's suggested renewal window for `host`'s current certificate (ARI). `None` when
    /// there is no certificate, the CA does not support ARI or cannot be asked right now.
    ///
    /// The CA is asked again only once its `Retry-After` has passed (RFC 9773 §4.3); until
    /// then, or until the certificate changes, its last answer stands.
    pub async fn renewal_window(&self, host: &str) -> Option<RenewalWindow> {
        let key = self.certs.get(host)?;
        let id = CertificateIdentifier::try_from(key.cert.first()?).ok()?;
        let cert = id.to_string();
        let now = tokio::time::Instant::now();
        if let Some(answer) = self.ari_answers().get(host)
            && answer.cert == cert
            && answer.until > now
        {
            return answer.window;
        }
        let (window, ask_again) = match self.ca.renewal_window(&id).await {
            Ok(Some((window, retry_after))) => (
                Some(window),
                retry_after.clamp(Self::ARI_RETRY_MIN, Self::ARI_RETRY_MAX),
            ),
            Ok(None) => (None, Self::ARI_RETRY_MAX),
            Err(error) => {
                tracing::debug!(host, %error, "cannot fetch ACME renewal information");
                return None;
            }
        };
        self.ari_answers().insert(
            host.to_owned(),
            AriAnswer {
                cert,
                window,
                until: now + ask_again,
            },
        );
        window
    }

    fn ari_answers(&self) -> std::sync::MutexGuard<'_, HashMap<String, AriAnswer>> {
        // The map holds plain data and no invariants a panic could break.
        self.ari
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn run(&self, host: &str) -> Result<Validity, IssueFailure> {
        let mut order = self.ca.new_order(host).await?;
        let challenges = order.http01_challenges().await?;
        // The answers are served before the CA is told to look for them, and withdrawn when this
        // function returns or is cancelled, whatever the outcome.
        let published = Published::new(self.challenges.clone(), &challenges);
        for challenge in &challenges {
            order.set_ready(challenge).await?;
        }

        match order.poll_ready(&self.retry).await? {
            OrderStatus::Ready => {}
            status => {
                let detail = match order.validation_problem().await {
                    Some(problem) => CaDetail(&problem).to_string(),
                    None => format!("the order became {status:?}"),
                };
                return Err(IssueFailure::Validation { detail });
            }
        }
        let key_pem = order.finalize().await?;
        let chain_pem = order.poll_certificate(&self.retry).await?;
        drop(published);

        let (certs, host) = (self.certs.clone(), host.to_owned());
        tokio::task::spawn_blocking(move || certs.insert(&host, chain_pem, key_pem))
            .await
            .expect("certificate insert does not panic")
            .map_err(IssueFailure::from)
    }
}

#[cfg(test)]
#[path = "issue_tests.rs"]
mod tests;
