//! The certificates page (ADR-0036, ADR-0051 #2): every host hoster manages a certificate for,
//! with its issuer, end of validity, last ACME error, next attempt and the advisory DNS check,
//! under a header naming hoster's own public addresses, which that check compares against.
//!
//! Two actions, both HTMX posts:
//!
//! - **Check DNS now** (`POST /certificates/dns`) compares one host's A/AAAA records with the
//!   public addresses again and swaps the result into the host's DNS cell. The result is also
//!   recorded in the orchestrator's certificate status until the next attempt reports its own.
//! - **Retry now** (`POST /certificates/retry`) asks the ACME task to attempt the host now. It is
//!   refused, with the time left, while the host's persisted backoff runs (ADR-0035 #5), so the
//!   button cannot spend the CA's rate limits.
//!
//! The handlers reach the orchestrator, the served certificates, the DNS check and the backoffs
//! through [`CertificateControl`], so the tests use a fake.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use askama::Template;
use axum::Extension;
use axum::extract::Form;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::auth::Session;
use super::pages::{Layout, Page, Section};
use super::stacks::Outcome;
use super::ui::{CertState, Pill, Tone};
use crate::acme::{Backoff, BackoffStore, CertMap, DnsCheck, DnsMismatch, DnsVerdict, Validity};
use crate::orchestrator::{CertStatus, Orchestrator};
use crate::runtime::ContainerRuntime;

/// One host's certificate, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertRow {
    pub host: String,
    /// The served certificate's issuer and validity, if the host has one.
    pub issuer: Option<String>,
    pub validity: Option<Validity>,
    /// What the ACME task reported since startup.
    pub status: CertStatus,
    /// The persisted backoff, while the host is failing.
    pub backoff: Option<Backoff>,
}

/// Everything the page shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertOverview {
    /// Where hoster's public addresses come from (`the A/AAAA records of admin.example.com`).
    pub source: String,
    /// hoster's public addresses as last resolved; empty when unknown.
    pub public: BTreeSet<IpAddr>,
    /// Every managed host, sorted.
    pub hosts: Vec<CertRow>,
}

/// Why an action was refused.
#[derive(Debug, thiserror::Error)]
pub enum CertActionError {
    #[error("hoster manages no certificate for {0}")]
    UnknownHost(String),
    #[error("{host} is backing off after a failed attempt")]
    BackingOff {
        host: String,
        retry_at: SystemTime,
        left: Duration,
    },
    #[error("the ACME task is not running")]
    AcmeStopped,
    #[error("cannot read the ACME backoffs")]
    Store(#[source] anyhow::Error),
}

/// What the certificates page needs. Dyn-compatible, so tests use a fake.
#[async_trait::async_trait]
pub trait CertificateControl: Send + Sync + 'static {
    /// The public addresses and every managed host.
    async fn overview(&self) -> CertOverview;

    /// Compares `host`'s DNS with the public addresses now, and records the outcome.
    async fn check_dns(&self, host: &str) -> Result<DnsVerdict, CertActionError>;

    /// Asks the ACME task to attempt `host` now, unless its backoff has not elapsed.
    async fn retry(&self, host: &str) -> Result<(), CertActionError>;
}

/// The production wiring: the orchestrator's certificate status, the served certificates, the
/// DNS check shared with the ACME task and the backoffs it persists.
pub struct CertificateAccess<R: ContainerRuntime> {
    pub orchestrator: Arc<Orchestrator<R>>,
    pub certs: CertMap,
    pub dns: DnsCheck,
    pub backoffs: Arc<dyn BackoffStore>,
    /// The admin host, which always has a certificate (ADR-0036).
    pub admin_host: String,
}

impl<R: ContainerRuntime> CertificateAccess<R> {
    /// The host in canonical form, if hoster manages a certificate for it.
    fn managed(&self, host: &str) -> Result<String, CertActionError> {
        let host = host.trim().to_ascii_lowercase();
        if host == self.admin_host || self.orchestrator.certificates().contains_key(&host) {
            Ok(host)
        } else {
            Err(CertActionError::UnknownHost(host))
        }
    }
}

#[async_trait::async_trait]
impl<R: ContainerRuntime> CertificateControl for CertificateAccess<R> {
    async fn overview(&self) -> CertOverview {
        let mut status = self.orchestrator.certificates();
        status.entry(self.admin_host.clone()).or_default();
        let mut backoffs = match self.backoffs.load().await {
            Ok(backoffs) => backoffs,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "cannot read the ACME backoffs"
                );
                Default::default()
            }
        };
        let hosts = status
            .into_iter()
            .map(|(host, status)| {
                let entry = self.certs.entry(&host);
                CertRow {
                    issuer: entry.as_ref().map(|e| e.issuer.clone()),
                    validity: entry.map(|e| e.validity),
                    backoff: backoffs.remove(&host),
                    status,
                    host,
                }
            })
            .collect();
        CertOverview {
            source: self.dns.source().to_string(),
            public: self.dns.expected(),
            hosts,
        }
    }

    async fn check_dns(&self, host: &str) -> Result<DnsVerdict, CertActionError> {
        let host = self.managed(host)?;
        let verdict = self.dns.check(&host).await;
        self.orchestrator
            .record_dns_check(&host, &self.admin_host, &verdict);
        Ok(verdict)
    }

    async fn retry(&self, host: &str) -> Result<(), CertActionError> {
        let host = self.managed(host)?;
        let backoffs = self.backoffs.load().await.map_err(CertActionError::Store)?;
        if let Some(backoff) = backoffs.get(&host)
            && let Some(left) = backoff.remaining(SystemTime::now())
        {
            return Err(CertActionError::BackingOff {
                host,
                retry_at: backoff.retry_at,
                left,
            });
        }
        if self.orchestrator.retry_certificate(&host) {
            Ok(())
        } else {
            Err(CertActionError::AcmeStopped)
        }
    }
}

/// The certificates page's handlers, sharing the control as a request extension.
#[derive(Clone)]
pub struct Certificates(Arc<dyn CertificateControl>);

impl From<Arc<dyn CertificateControl>> for Certificates {
    fn from(control: Arc<dyn CertificateControl>) -> Self {
        Self(control)
    }
}

/// The forms that name a host.
#[derive(Deserialize)]
pub struct HostForm {
    #[serde(default)]
    host: String,
}

impl Certificates {
    /// `GET /certificates`.
    pub async fn page(
        Extension(certificates): Extension<Certificates>,
        Extension(session): Extension<Session>,
    ) -> Response {
        let overview = certificates.0.overview().await;
        Page(CertificatesPage::new(
            Layout::new(Section::Certificates, &session),
            &overview,
            SystemTime::now(),
        ))
        .into_response()
    }

    /// `POST /certificates/dns`: the host's DNS cell, freshly compared.
    pub async fn check_dns(
        Extension(certificates): Extension<Certificates>,
        Form(form): Form<HostForm>,
    ) -> Response {
        match certificates.0.check_dns(&form.host).await {
            Ok(verdict) => Page(DnsCell {
                dns: DnsView::from(&verdict),
            })
            .into_response(),
            Err(err) => Self::refused(&err, SystemTime::now()).into_response(),
        }
    }

    /// `POST /certificates/retry`: asks for an attempt now, outside the host's backoff.
    pub async fn retry(
        Extension(certificates): Extension<Certificates>,
        Form(form): Form<HostForm>,
    ) -> Response {
        match certificates.0.retry(&form.host).await {
            Ok(()) => Outcome::success(format!(
                "Asked the ACME task to try {} now. It requests a certificate only if the host \
                 has none or its certificate is due for renewal.",
                form.host.trim().to_ascii_lowercase()
            ))
            .with_link("/certificates".to_owned(), "Reload the certificates")
            .into_response(),
            Err(err) => Self::refused(&err, SystemTime::now()).into_response(),
        }
    }

    /// A refused action as an error message.
    fn refused(err: &CertActionError, now: SystemTime) -> Outcome {
        match err {
            CertActionError::UnknownHost(_) => {
                Outcome::error(StatusCode::NOT_FOUND, Outcome::sentence(&err.to_string()))
            }
            CertActionError::BackingOff { host, retry_at, .. } => Outcome::error(
                StatusCode::CONFLICT,
                format!(
                    "Not retrying {host}: its backoff after a failed attempt has {} left (until \
                     {}). Retrying earlier would spend the CA's rate limits.",
                    Left(*retry_at, now),
                    humantime::format_rfc3339_seconds(*retry_at),
                ),
            ),
            CertActionError::AcmeStopped => Outcome::error(
                StatusCode::SERVICE_UNAVAILABLE,
                Outcome::sentence(&err.to_string()),
            ),
            CertActionError::Store(source) => {
                tracing::error!(
                    error = format!("{source:#}"),
                    "cannot read the ACME backoffs"
                );
                Outcome::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Outcome::sentence(&err.to_string()),
                )
            }
        }
    }
}

/// The time left until `.0` at `.1`, to the second (`4m 30s`).
struct Left(SystemTime, SystemTime);

impl std::fmt::Display for Left {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let left = self.0.duration_since(self.1).unwrap_or_default();
        let left = Duration::from_secs(left.as_secs().max(1));
        write!(f, "{}", humantime::format_duration(left))
    }
}

/// What the DNS cell says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DnsState {
    Matches,
    Mismatch,
    /// hoster's own addresses are unknown, so nothing was compared.
    Unknown,
    Unchecked,
}

impl From<DnsState> for Tone {
    fn from(state: DnsState) -> Self {
        match state {
            DnsState::Matches => Tone::Ok,
            DnsState::Mismatch => Tone::Warn,
            DnsState::Unknown | DnsState::Unchecked => Tone::Neutral,
        }
    }
}

/// The DNS cell of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsView {
    state: DnsState,
    /// The host's addresses, comma-separated, for a mismatch.
    observed: String,
    expected: String,
}

impl DnsView {
    const UNCHECKED: DnsView = DnsView {
        state: DnsState::Unchecked,
        observed: String::new(),
        expected: String::new(),
    };

    /// The verdict as a pill; `None` when there is none to show.
    fn pill(&self) -> Option<Pill> {
        match self.state {
            DnsState::Matches => Some(Tone::from(self.state).pill("ok")),
            DnsState::Mismatch => Some(Tone::from(self.state).pill("mismatch")),
            DnsState::Unknown | DnsState::Unchecked => None,
        }
    }

    fn list(addresses: &[IpAddr]) -> String {
        addresses
            .iter()
            .map(IpAddr::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl From<&DnsMismatch> for DnsView {
    fn from(mismatch: &DnsMismatch) -> Self {
        DnsView {
            state: DnsState::Mismatch,
            observed: Self::list(&mismatch.observed),
            expected: Self::list(&mismatch.expected),
        }
    }
}

impl From<&DnsVerdict> for DnsView {
    fn from(verdict: &DnsVerdict) -> Self {
        match verdict {
            DnsVerdict::Matches => DnsView {
                state: DnsState::Matches,
                ..Self::UNCHECKED
            },
            DnsVerdict::Mismatch(mismatch) => Self::from(mismatch),
            DnsVerdict::Unknown => DnsView {
                state: DnsState::Unknown,
                ..Self::UNCHECKED
            },
        }
    }
}

impl DnsView {
    /// The cell for what the ACME task and "Check DNS now" recorded in `status`. An attempt
    /// skips the comparison while hoster's own addresses are unknown (`public_known` false) and
    /// then reports no mismatch, so that is shown as unknown rather than ok.
    fn of_status(status: &CertStatus, public_known: bool) -> Self {
        match (&status.dns_mismatch, status.dns_checked) {
            (Some(mismatch), _) => Self::from(mismatch),
            (None, true) if public_known => Self::from(&DnsVerdict::Matches),
            (None, true) => Self::from(&DnsVerdict::Unknown),
            (None, false) => Self::UNCHECKED,
        }
    }
}

/// One row of the table.
#[derive(Debug)]
struct RowView {
    /// The row's position, for element ids (host names contain dots).
    index: usize,
    host: String,
    issuer: Option<String>,
    not_after: Option<String>,
    error: Option<String>,
    next_attempt: String,
    dns: DnsView,
}

impl RowView {
    fn new(index: usize, row: &CertRow, public_known: bool, now: SystemTime) -> Self {
        let not_after = row.validity.map(|v| v.not_after).or(row.status.not_after);
        let retry_at = row.backoff.map(|b| b.retry_at).or(row.status.retry_at);
        let next_attempt = match (retry_at, row.validity) {
            (Some(at), _) if at > now => format!(
                "in {} ({}), after a failure",
                Left(at, now),
                humantime::format_rfc3339_seconds(at)
            ),
            (Some(_), _) => "now, after a failure".to_owned(),
            (None, Some(validity)) if !validity.is_expired_at(now) => format!(
                "renewal from {}",
                humantime::format_rfc3339_seconds(validity.renew_at())
            ),
            (None, _) => "pending".to_owned(),
        };
        Self {
            index,
            host: row.host.clone(),
            issuer: row.issuer.clone(),
            not_after: not_after.map(|at| humantime::format_rfc3339_seconds(at).to_string()),
            error: row.status.error.clone(),
            next_attempt,
            dns: DnsView::of_status(&row.status, public_known),
        }
    }

    /// The "Valid until" cell of a host without a certificate: pending, or failing once an
    /// attempt reported an error.
    fn pending_pill(&self) -> Pill {
        Tone::from(CertState::unissued(self.error.as_deref())).pill("TLS pending")
    }
}

/// `GET /certificates`.
#[derive(Template)]
#[template(path = "certificates.html")]
pub struct CertificatesPage {
    layout: Layout,
    source: String,
    /// hoster's public addresses, comma-separated; empty when unknown.
    public: String,
    rows: Vec<RowView>,
}

impl CertificatesPage {
    fn new(layout: Layout, overview: &CertOverview, now: SystemTime) -> Self {
        let public: Vec<IpAddr> = overview.public.iter().copied().collect();
        Self {
            layout,
            source: overview.source.clone(),
            public: DnsView::list(&public),
            rows: overview
                .hosts
                .iter()
                .enumerate()
                .map(|(index, row)| RowView::new(index, row, !public.is_empty(), now))
                .collect(),
        }
    }
}

/// The partial `POST /certificates/dns` answers with.
#[derive(Template)]
#[template(path = "cert_dns.html")]
pub struct DnsCell {
    dns: DnsView,
}

#[cfg(test)]
pub(super) mod tests;
