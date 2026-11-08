//! The certificates page (ADR-0036, ADR-0051 #2): every host hoster manages a certificate for,
//! with its issuer, end of validity, last ACME error, next attempt and the advisory DNS check,
//! under a header naming hoster's own public addresses, which that check compares against.
//!
//! The page (epic 8bf) has summary tiles, an alert for the worst problem, and the table sorted
//! by expiry with the problems first. Four actions, all HTMX posts:
//!
//! - **check dns** (`POST /certificates/dns`) compares one host's A/AAAA records with the
//!   public addresses again and swaps the result into the host's DNS cell. The result is also
//!   recorded in the orchestrator's certificate status until the next attempt reports its own.
//! - **retry** (`POST /certificates/retry`) asks the ACME task to attempt the host now. It is
//!   refused, with the time left, while the host's persisted backoff runs (ADR-0035 #5), so the
//!   button cannot spend the CA's rate limits.
//! - **Check all DNS** (`POST /certificates/dns/all`) runs that check for every host at once and
//!   answers with the page body (tiles, alert and table) re-rendered under a summary flash.
//! - **Retry failing** (`POST /certificates/retry/failing`) asks for an attempt for every
//!   failing host (the ones the navigation counts), each gated on its backoff like **retry**.
//!
//! The handlers reach the orchestrator, the served certificates, the DNS check and the backoffs
//! through [`CertificateControl`], so the tests use a fake.
//!
//! The dashboard shows the certificates at a glance ([`CertGlance`], from
//! [`Certificates::glance`]): a KPI tile and the hosts soonest to expire, with the same status
//! words as this page's table.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use askama::Template;
use axum::Extension;
use axum::extract::Form;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::future::join_all;
use serde::Deserialize;

use super::pages::{Flash, FlashKind, Layout, Page, Section, Shell};
use super::stacks::Outcome;
use super::ui::{CertState, Kpi, Meter, Pill, Tone};
use crate::acme::{
    Backoff, BackoffStore, Backoffs, CertMap, DnsCheck, DnsMismatch, DnsVerdict, Validity,
};
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
    /// Who the certificate is for.
    pub owner: CertOwner,
}

/// Who a host's certificate is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertOwner {
    /// hoster's own admin host (ADR-0036).
    Admin,
    /// The stacks whose routes name the host; empty for a host the ACME task reported on that
    /// no stack routes any more.
    Stacks(Vec<String>),
}

impl CertRow {
    /// Whether the host needs attention at `now`: its last attempt failed, it has no certificate
    /// yet (the page's "TLS pending"), or the certificate it serves has expired (which survives
    /// a restart, when the recorded error does not). The navigation counts these.
    pub fn is_failing(&self, now: SystemTime) -> bool {
        self.status.error.is_some()
            || (self.validity.is_none() && self.status.not_after.is_none())
            || self.validity.is_some_and(|v| v.is_expired_at(now))
    }
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
    /// The ACME directory hoster orders from.
    pub directory: AcmeDirectory,
}

/// The ACME directory in `acme.directory_url`, as the ACME tile names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcmeDirectory {
    LetsEncrypt,
    LetsEncryptStaging,
    /// Any other CA (a test CA such as Pebble, or a commercial one), by its URL.
    Custom(String),
}

impl AcmeDirectory {
    const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";
    const LETS_ENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

    /// The directory URL.
    pub fn url(&self) -> &str {
        match self {
            AcmeDirectory::LetsEncrypt => Self::LETS_ENCRYPT,
            AcmeDirectory::LetsEncryptStaging => Self::LETS_ENCRYPT_STAGING,
            AcmeDirectory::Custom(url) => url,
        }
    }
}

impl From<&str> for AcmeDirectory {
    fn from(url: &str) -> Self {
        match url.trim_end_matches('/') {
            Self::LETS_ENCRYPT => AcmeDirectory::LetsEncrypt,
            Self::LETS_ENCRYPT_STAGING => AcmeDirectory::LetsEncryptStaging,
            _ => AcmeDirectory::Custom(url.to_owned()),
        }
    }
}

/// The tile's short name: `letsencrypt · prod`, `letsencrypt · staging`, or the custom
/// directory's host.
impl std::fmt::Display for AcmeDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcmeDirectory::LetsEncrypt => f.write_str("letsencrypt · prod"),
            AcmeDirectory::LetsEncryptStaging => f.write_str("letsencrypt · staging"),
            AcmeDirectory::Custom(url) => {
                let rest = url.split_once("://").map_or(url.as_str(), |(_, rest)| rest);
                let host = rest.split(['/', '?']).next().unwrap_or(rest);
                write!(f, "custom · {host}")
            }
        }
    }
}

impl CertOverview {
    /// How many hosts are failing at `now` (see [`CertRow::is_failing`]).
    pub fn failing(&self, now: SystemTime) -> usize {
        self.hosts.iter().filter(|row| row.is_failing(now)).count()
    }
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

    /// How many hosts are failing, for the navigation; see [`CertOverview::failing`].
    async fn failing(&self) -> usize {
        self.overview().await.failing(SystemTime::now())
    }

    /// Compares `host`'s DNS with the public addresses now, and records the outcome.
    async fn check_dns(&self, host: &str) -> Result<DnsVerdict, CertActionError>;

    /// Asks the ACME task to attempt `host` now, unless its backoff has not elapsed.
    async fn retry(&self, host: &str) -> Result<(), CertActionError>;

    /// Runs [`check_dns`](Self::check_dns) for every managed host at once.
    async fn check_all_dns(&self) -> DnsSweep {
        let overview = self.overview().await;
        let checks = overview
            .hosts
            .iter()
            .map(|row| async move { (row.host.clone(), self.check_dns(&row.host).await) });
        let mut sweep = DnsSweep::default();
        for (host, verdict) in join_all(checks).await {
            match verdict {
                Ok(DnsVerdict::Matches) => sweep.matching += 1,
                Ok(DnsVerdict::Mismatch(_)) => sweep.mismatched.push(host),
                Ok(DnsVerdict::Unknown) => sweep.unknown += 1,
                // A host that stopped being managed since the overview was read.
                Err(_) => {}
            }
        }
        sweep
    }

    /// Runs [`retry`](Self::retry) for every host failing at `now` (see
    /// [`CertRow::is_failing`]), the ones still backing off being refused.
    async fn retry_failing(&self, now: SystemTime) -> Result<RetrySweep, CertActionError> {
        let overview = self.overview().await;
        let mut sweep = RetrySweep::default();
        for row in overview.hosts.iter().filter(|row| row.is_failing(now)) {
            match self.retry(&row.host).await {
                Ok(()) => sweep.queued.push(row.host.clone()),
                Err(CertActionError::BackingOff { host, retry_at, .. }) => {
                    sweep.backing_off.push((host, retry_at));
                }
                Err(CertActionError::UnknownHost(_)) => {}
                Err(err @ (CertActionError::AcmeStopped | CertActionError::Store(_))) => {
                    return Err(err);
                }
            }
        }
        Ok(sweep)
    }
}

/// What **Check all DNS** found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DnsSweep {
    /// How many hosts point at hoster.
    pub matching: usize,
    /// The hosts that resolve elsewhere.
    pub mismatched: Vec<String>,
    /// How many could not be compared, hoster's own addresses being unknown.
    pub unknown: usize,
}

impl From<&DnsSweep> for Flash {
    fn from(sweep: &DnsSweep) -> Self {
        let total = sweep.matching + sweep.mismatched.len() + sweep.unknown;
        let mut message = format!(
            "Checked DNS for {total} {}: {} {} here",
            if total == 1 { "host" } else { "hosts" },
            sweep.matching,
            if sweep.matching == 1 {
                "points"
            } else {
                "point"
            },
        );
        if !sweep.mismatched.is_empty() {
            message.push_str(&format!(
                ", {} {} elsewhere ({})",
                sweep.mismatched.len(),
                if sweep.mismatched.len() == 1 {
                    "resolves"
                } else {
                    "resolve"
                },
                sweep.mismatched.join(", ")
            ));
        }
        if sweep.unknown > 0 {
            message.push_str(&format!(
                ", {} could not be compared because hoster's public addresses are unknown",
                sweep.unknown
            ));
        }
        message.push('.');
        Flash {
            kind: if sweep.mismatched.is_empty() && sweep.unknown == 0 {
                FlashKind::Success
            } else {
                FlashKind::Info
            },
            message,
        }
    }
}

/// What **Retry failing** asked for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetrySweep {
    /// The hosts the ACME task was asked to attempt now.
    pub queued: Vec<String>,
    /// The hosts refused because their backoff runs, with its end.
    pub backing_off: Vec<(String, SystemTime)>,
}

impl RetrySweep {
    /// The outcome shown under the page header at `now`.
    fn outcome(&self, now: SystemTime) -> Outcome {
        let refused = self
            .backing_off
            .iter()
            .map(|(host, at)| format!("{host} ({} left)", Left(*at, now)))
            .collect::<Vec<_>>()
            .join(", ");
        match (self.queued.is_empty(), self.backing_off.is_empty()) {
            (true, true) => Outcome::success("No host is failing, so there is nothing to retry."),
            (true, false) => Outcome::error(
                StatusCode::CONFLICT,
                format!(
                    "Not retrying: every failing host is backing off after a failed attempt: \
                     {refused}. Retrying earlier would spend the CA's rate limits."
                ),
            ),
            (false, _) => {
                let mut message = format!(
                    "Asked the ACME task to try {} now: {}.",
                    if self.queued.len() == 1 {
                        "1 host".to_owned()
                    } else {
                        format!("{} hosts", self.queued.len())
                    },
                    self.queued.join(", ")
                );
                if !refused.is_empty() {
                    message.push_str(&format!(
                        " Not retrying the ones backing off after a failed attempt: {refused}."
                    ));
                }
                Outcome::success(message)
                    .with_link("/certificates".to_owned(), "Reload the certificates")
            }
        }
    }
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
    /// The configured ACME directory.
    pub directory: AcmeDirectory,
}

impl<R: ContainerRuntime> CertificateAccess<R> {
    /// Every managed host (the admin host always), with the backoff `backoffs` holds for it.
    fn rows(&self, mut backoffs: Backoffs) -> Vec<CertRow> {
        let mut status = self.orchestrator.certificates();
        let mut owners = self.orchestrator.certificate_owners();
        status.entry(self.admin_host.clone()).or_default();
        status
            .into_iter()
            .map(|(host, status)| {
                let entry = self.certs.entry(&host);
                let owner = if host == self.admin_host {
                    CertOwner::Admin
                } else {
                    CertOwner::Stacks(
                        owners
                            .remove(&host)
                            .unwrap_or_default()
                            .into_iter()
                            .collect(),
                    )
                };
                CertRow {
                    owner,
                    issuer: entry.as_ref().map(|e| e.issuer.clone()),
                    validity: entry.map(|e| e.validity),
                    backoff: backoffs.remove(&host),
                    status,
                    host,
                }
            })
            .collect()
    }

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
        let backoffs = match self.backoffs.load().await {
            Ok(backoffs) => backoffs,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "cannot read the ACME backoffs"
                );
                Default::default()
            }
        };
        CertOverview {
            source: self.dns.source().to_string(),
            public: self.dns.expected(),
            hosts: self.rows(backoffs),
            directory: self.directory.clone(),
        }
    }

    /// The rows without their backoffs, which only the page shows: no store query.
    async fn failing(&self) -> usize {
        let now = SystemTime::now();
        self.rows(Backoffs::new())
            .iter()
            .filter(|row| row.is_failing(now))
            .count()
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
    /// The certificates at `now`, for the dashboard.
    pub(super) async fn glance(&self, now: SystemTime) -> CertGlance {
        CertGlance::new(&self.0.overview().await, now)
    }

    /// `GET /certificates`.
    pub async fn page(Extension(certificates): Extension<Certificates>, shell: Shell) -> Response {
        let overview = certificates.0.overview().await;
        Page(CertificatesPage::new(
            shell.layout(Section::Certificates),
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

    /// `POST /certificates/dns/all`: checks every host's DNS, then answers with the page body
    /// re-rendered under a summary of what was found.
    pub async fn check_all_dns(Extension(certificates): Extension<Certificates>) -> Response {
        let sweep = certificates.0.check_all_dns().await;
        let overview = certificates.0.overview().await;
        let mut body = CertBody::new(&overview, SystemTime::now());
        body.flash = Some(Flash::from(&sweep));
        Page(CertBodyPartial { body }).into_response()
    }

    /// `POST /certificates/retry/failing`: asks for an attempt for every failing host outside
    /// its backoff.
    pub async fn retry_failing(Extension(certificates): Extension<Certificates>) -> Response {
        let now = SystemTime::now();
        match certificates.0.retry_failing(now).await {
            Ok(sweep) => sweep.outcome(now).into_response(),
            Err(err) => Self::refused(&err, now).into_response(),
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

/// Where a row stands, for its status pill, the tiles and the sort order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    /// The last attempt failed (the certificate, if any, is still served).
    Failed,
    /// Served, but past its end of validity, with no failure recorded since startup.
    Expired,
    /// Served, with less than [`CertState::WARN_WITHIN`] left.
    Expiring,
    /// None served yet, and no attempt failed.
    Pending,
    /// Served and due for renewal (ADR-0015), not yet within the warning window.
    Renewing,
    /// Served and not due for renewal.
    Valid,
}

impl Standing {
    fn of(row: &CertRow, not_after: Option<SystemTime>, now: SystemTime) -> Self {
        if row.status.error.is_some() {
            return Standing::Failed;
        }
        let Some(not_after) = not_after else {
            return Standing::Pending;
        };
        match CertState::from(not_after.duration_since(now).unwrap_or_default()) {
            CertState::Expired => Standing::Expired,
            CertState::Expiring => Standing::Expiring,
            _ if row.validity.is_some_and(|v| now >= v.renew_at()) => Standing::Renewing,
            _ => Standing::Valid,
        }
    }

    /// Whether the row sorts with the problems at the top of the table.
    fn is_problem(self) -> bool {
        matches!(
            self,
            Standing::Failed | Standing::Expired | Standing::Expiring | Standing::Pending
        )
    }

    fn tone(self) -> Tone {
        match self {
            Standing::Failed | Standing::Expired => Tone::Fail,
            Standing::Expiring => Tone::Warn,
            Standing::Pending | Standing::Renewing => Tone::Progress,
            Standing::Valid => Tone::Ok,
        }
    }

    fn pill(self) -> Pill {
        self.tone().pill(match self {
            Standing::Failed => "failed",
            Standing::Expired => "expired",
            Standing::Expiring => "expiring",
            Standing::Pending => "pending",
            Standing::Renewing => "renewing",
            Standing::Valid => "valid",
        })
    }
}

/// One stack that routes a host, linked to its page.
#[derive(Debug)]
struct StackLink {
    name: String,
    href: String,
}

/// The "Expires" cell of a host with a certificate.
#[derive(Debug)]
struct Expiry {
    /// RFC 3339, for the `datetime`.
    at: String,
    /// The date alone (`2026-12-15`).
    date: String,
    /// The time of day under it (`14:30Z`).
    time: String,
    /// `37d left`, `expires today`, `expired 3d ago`.
    left: String,
    meter: Meter,
}

impl Expiry {
    const DAY: u64 = 24 * 60 * 60;
    /// The meter's scale: Let's Encrypt's certificate lifetime.
    const LIFETIME_DAYS: f64 = 90.0;

    fn new(not_after: SystemTime, tone: Tone, now: SystemTime) -> Self {
        let at = humantime::format_rfc3339_seconds(not_after).to_string();
        let (days, left) = match not_after.duration_since(now) {
            Ok(left) => {
                let days = left.as_secs() / Self::DAY;
                (
                    days,
                    if days == 0 {
                        "expires today".to_owned()
                    } else {
                        format!("{days}d left")
                    },
                )
            }
            Err(past) => (
                0,
                match past.duration().as_secs() / Self::DAY {
                    0 => "expired today".to_owned(),
                    days => format!("expired {days}d ago"),
                },
            ),
        };
        Self {
            date: at.get(..10).unwrap_or(&at).to_owned(),
            time: at
                .get(11..16)
                .map_or_else(String::new, |hm| format!("{hm}Z")),
            meter: Meter::new(
                days as f64,
                Self::LIFETIME_DAYS,
                tone,
                format!("{days} of 90 days left"),
            ),
            at,
            left,
        }
    }
}

/// One row of the table.
#[derive(Debug)]
struct RowView {
    /// The row's position, for element ids (host names contain dots).
    index: usize,
    host: String,
    /// `None` for the admin host, which no stack owns.
    stacks: Option<Vec<StackLink>>,
    issuer: Option<String>,
    expiry: Option<Expiry>,
    not_after: Option<SystemTime>,
    standing: Standing,
    error: Option<AcmeError>,
    /// Consecutive failed attempts, while backing off.
    failures: Option<u32>,
    next_attempt: String,
    dns: DnsView,
    failing: bool,
}

impl RowView {
    fn new(row: &CertRow, public_known: bool, now: SystemTime) -> Self {
        let not_after = row.validity.map(|v| v.not_after).or(row.status.not_after);
        let retry_at = row.backoff.map(|b| b.retry_at).or(row.status.retry_at);
        let next_attempt = match (retry_at, row.validity) {
            (Some(at), _) if at > now => format!(
                "in {} ({}), after a failure",
                Left(at, now),
                humantime::format_rfc3339_seconds(at)
            ),
            (Some(_), _) => "now, after a failure".to_owned(),
            (None, Some(validity))
                if validity.renew_at() <= now && !validity.is_expired_at(now) =>
            {
                "renewal due now".to_owned()
            }
            (None, Some(validity)) if !validity.is_expired_at(now) => format!(
                "renewal from {}",
                humantime::format_rfc3339_seconds(validity.renew_at())
            ),
            (None, _) => "pending".to_owned(),
        };
        let standing = Standing::of(row, not_after, now);
        let stacks = match &row.owner {
            CertOwner::Admin => None,
            CertOwner::Stacks(stacks) => Some(
                stacks
                    .iter()
                    .map(|name| StackLink {
                        name: name.clone(),
                        href: format!("/stacks/{name}"),
                    })
                    .collect(),
            ),
        };
        Self {
            index: 0,
            host: row.host.clone(),
            stacks,
            issuer: row.issuer.clone(),
            expiry: not_after.map(|at| Expiry::new(at, standing.tone(), now)),
            not_after,
            standing,
            error: row.status.error.clone().map(AcmeError),
            failures: row.backoff.map(|b| b.failures),
            next_attempt,
            dns: DnsView::of_status(&row.status, public_known),
            failing: row.is_failing(now),
        }
    }

    /// The challenge type: hoster only uses HTTP-01 (ADR-0016).
    fn challenge(&self) -> &'static str {
        "http-01"
    }

    fn pill(&self) -> Pill {
        self.standing.pill()
    }

    /// The table order: problems first, then by end of validity (hosts without a certificate
    /// first), then by name.
    fn sort_key(&self) -> (bool, Option<SystemTime>, &str) {
        (!self.standing.is_problem(), self.not_after, &self.host)
    }

    /// `3 failed attempts`, or `a failed attempt` when the count is not known.
    fn attempts(&self) -> String {
        match self.failures {
            Some(1) | None => "a failed attempt".to_owned(),
            Some(n) => format!("{n} failed attempts"),
        }
    }
}

/// The alert above the table, for the worst problem: a host serving an expired certificate
/// (failing or not; after a restart the recorded error is gone but the expiry is not), else a
/// failing host whose certificate expires soon, else a host with no certificate after failed
/// attempts.
#[derive(Debug)]
struct AlertView {
    host: String,
    title: String,
    /// The last ACME error; `None` when no attempt failed since hoster started.
    error: Option<AcmeError>,
    next_attempt: String,
    /// Other hosts needing attention, below in the table.
    others: usize,
}

impl AlertView {
    /// The alert for `rows` (in table order) at `now`, if one is due.
    fn of(rows: &[RowView], now: SystemTime) -> Option<Self> {
        let failed = || rows.iter().filter(|row| row.standing == Standing::Failed);
        let expired = rows.iter().find_map(|row| {
            if !matches!(row.standing, Standing::Failed | Standing::Expired) {
                return None;
            }
            let past = now.duration_since(row.not_after?).ok()?;
            let title = match row.standing {
                Standing::Failed => format!(
                    "{}'s certificate expired {} ago and renewal is failing",
                    row.host,
                    Days(past)
                ),
                _ => format!("{}'s certificate expired {} ago", row.host, Days(past)),
            };
            Some((row, title))
        });
        let expiring = || {
            failed().find_map(|row| {
                let left = row.not_after?.duration_since(now).ok()?;
                (left < CertState::WARN_WITHIN).then(|| {
                    let title = format!(
                        "{} expires in {} and renewal is failing",
                        row.host,
                        Days(left)
                    );
                    (row, title)
                })
            })
        };
        let missing = || {
            failed().find(|row| row.not_after.is_none()).map(|row| {
                let title = format!("{} has no certificate after {}", row.host, row.attempts());
                (row, title)
            })
        };
        let (row, title) = expired.or_else(expiring).or_else(missing)?;
        Some(Self {
            host: row.host.clone(),
            title,
            error: row.error.clone(),
            next_attempt: row.next_attempt.clone(),
            others: rows.iter().filter(|r| r.failing).count().saturating_sub(1),
        })
    }
}

/// The last ACME error, as the page shows it: HTML-escaped, with a line-break opportunity
/// (`<wbr>`) after each colon, so a long problem URN (`urn:ietf:params:acme:error:rateLimited`)
/// wraps between its parts rather than mid-word.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AcmeError(String);

impl std::fmt::Display for AcmeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, part) in self.0.split(':').enumerate() {
            if i > 0 {
                f.write_str(":<wbr>")?;
            }
            match askama::filters::escape(part, askama::filters::Html) {
                Ok(safe) => write!(f, "{safe}")?,
                Err(never) => match never {},
            }
        }
        Ok(())
    }
}

impl askama::filters::HtmlSafe for AcmeError {}

/// A duration in whole days, for prose: `3 days`, `1 day`, `less than a day`.
struct Days(Duration);

impl std::fmt::Display for Days {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.as_secs() / Expiry::DAY {
            0 => f.write_str("less than a day"),
            1 => f.write_str("1 day"),
            days => write!(f, "{days} days"),
        }
    }
}

/// The tiles, the alert and the table: what **Check all DNS** re-renders.
#[derive(Debug)]
struct CertBody {
    tiles: Vec<Kpi>,
    alert: Option<AlertView>,
    rows: Vec<RowView>,
    /// The outcome of the action that re-rendered the body.
    flash: Option<Flash>,
}

impl CertBody {
    fn new(overview: &CertOverview, now: SystemTime) -> Self {
        let public_known = !overview.public.is_empty();
        let mut rows: Vec<RowView> = overview
            .hosts
            .iter()
            .map(|row| RowView::new(row, public_known, now))
            .collect();
        rows.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        for (index, row) in rows.iter_mut().enumerate() {
            row.index = index;
        }
        Self {
            tiles: Self::tiles(&rows, &overview.directory),
            alert: AlertView::of(&rows, now),
            rows,
            flash: None,
        }
    }

    fn tiles(rows: &[RowView], directory: &AcmeDirectory) -> Vec<Kpi> {
        let count = |standings: &[Standing]| {
            rows.iter()
                .filter(|row| standings.contains(&row.standing))
                .count()
        };
        let held = rows.iter().filter(|row| row.expiry.is_some()).count();
        let valid = count(&[Standing::Valid]);
        let due = count(&[Standing::Renewing, Standing::Expiring]);
        let pending = count(&[Standing::Pending]);
        let failed: Vec<&RowView> = rows
            .iter()
            .filter(|row| matches!(row.standing, Standing::Failed | Standing::Expired))
            .collect();
        let tone = |n: usize, tone: Tone| if n > 0 { tone } else { Tone::Neutral };

        let failed_sub = match failed.iter().max_by_key(|row| row.failures.unwrap_or(0)) {
            None => "none failing".to_owned(),
            Some(worst) => {
                let attempts = match worst.failures {
                    Some(n) if failed.len() == 1 => format!("{n} attempts · "),
                    Some(n) => format!("most: {n} attempts · "),
                    None => String::new(),
                };
                format!("{attempts}{}", worst.host)
            }
        };
        vec![
            Kpi::new(
                "held",
                held,
                format!("of {} {}", rows.len(), if rows.len() == 1 { "host" } else { "hosts" }),
            )
            .title("Hosts with a certificate served"),
            Kpi::new("valid", valid, "renewal not due yet")
                .tone(tone(valid, Tone::Ok))
                .title("More than a third of the certificate's lifetime left: 30 days of a 90-day certificate"),
            Kpi::new(
                "renewing / pending",
                due + pending,
                format!("{due} due for renewal · {pending} awaiting a first"),
            )
            .tone(tone(due + pending, Tone::Progress)),
            Kpi::new("failed", failed.len(), failed_sub).tone(tone(failed.len(), Tone::Fail)),
            Kpi::new("acme", directory, "http-01 · renews with ⅓ of lifetime left")
                .title(format!("ACME directory {}", directory.url())),
        ]
    }

    /// How many hosts **Retry failing** would ask for.
    fn failing(&self) -> usize {
        self.rows.iter().filter(|row| row.failing).count()
    }
}

/// The certificates at a glance, for the dashboard: a KPI tile of the valid hosts out of all,
/// and a card of the hosts soonest to expire, every host that needs attention among them.
#[derive(Debug, Default)]
pub(super) struct CertGlance {
    /// Every managed host.
    pub(super) total: usize,
    /// Hosts serving a certificate that is neither failing nor expiring soon.
    pub(super) valid: usize,
    /// Hosts the navigation counts as failing (see [`CertRow::is_failing`]).
    pub(super) failing: usize,
    /// Hosts whose certificate expires within [`CertState::WARN_WITHIN`], and not failing.
    pub(super) expiring: usize,
    /// The hosts the card lists, soonest expiry first (hosts without a certificate before all).
    pub(super) rows: Vec<GlanceRow>,
    /// The valid hosts the card leaves out.
    pub(super) more_valid: usize,
}

/// One host on the dashboard's certificates card.
#[derive(Debug)]
pub(super) struct GlanceRow {
    pub(super) host: String,
    pub(super) pill: Pill,
    /// `37d left`, `expired 3d ago`; `no certificate yet` without one.
    pub(super) left: String,
    /// The end of validity in RFC 3339, for the tooltip; empty without a certificate.
    pub(super) at: String,
    /// Days left out of a 90-day lifetime; `None` without a certificate.
    pub(super) meter: Option<Meter>,
}

impl CertGlance {
    /// The card lists at least this many hosts, when there are as many.
    const SHOWN: usize = 6;

    fn new(overview: &CertOverview, now: SystemTime) -> Self {
        let mut rows: Vec<RowView> = overview
            .hosts
            .iter()
            .map(|row| RowView::new(row, false, now))
            .collect();
        rows.sort_by(|a, b| (a.not_after, &a.host).cmp(&(b.not_after, &b.host)));
        let fine = |row: &RowView| matches!(row.standing, Standing::Valid | Standing::Renewing);
        let failing = rows.iter().filter(|row| row.failing).count();
        let expiring = rows
            .iter()
            .filter(|row| !row.failing && row.standing == Standing::Expiring)
            .count();
        let valid = rows.iter().filter(|row| fine(row)).count();
        let total = rows.len();
        // Every host that needs a look is listed, however many; valid ones fill up the rest.
        let (shown, rest): (Vec<_>, Vec<_>) = rows
            .into_iter()
            .enumerate()
            .partition(|(index, row)| *index < Self::SHOWN || !fine(row));
        Self {
            total,
            valid,
            failing,
            expiring,
            more_valid: rest.len(),
            rows: shown
                .into_iter()
                .map(|(_, row)| GlanceRow {
                    pill: row.pill(),
                    left: row
                        .expiry
                        .as_ref()
                        .map_or_else(|| "no certificate yet".to_owned(), |e| e.left.clone()),
                    at: row
                        .expiry
                        .as_ref()
                        .map_or_else(String::new, |e| e.at.clone()),
                    meter: row.expiry.map(|e| e.meter),
                    host: row.host,
                })
                .collect(),
        }
    }

    /// The dashboard's tile: valid hosts out of all, in the tone of the worst.
    pub(super) fn kpi(&self) -> Kpi {
        let tone = if self.failing > 0 {
            Tone::Fail
        } else if self.expiring > 0 {
            Tone::Warn
        } else if self.total > 0 && self.valid == self.total {
            Tone::Ok
        } else {
            Tone::Neutral
        };
        Kpi::new(
            "certificates",
            format!("{} / {}", self.valid, self.total),
            format!("{} failing · {} expiring", self.failing, self.expiring),
        )
        .tone(tone)
        .title(format!(
            "Valid certificates out of every host hoster manages; expiring means less than {} left",
            Days(CertState::WARN_WITHIN)
        ))
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
    /// The ACME directory's URL.
    directory: String,
    body: CertBody,
}

impl CertificatesPage {
    fn new(layout: Layout, overview: &CertOverview, now: SystemTime) -> Self {
        let public: Vec<IpAddr> = overview.public.iter().copied().collect();
        Self {
            layout,
            source: overview.source.clone(),
            public: DnsView::list(&public),
            directory: overview.directory.url().to_owned(),
            body: CertBody::new(overview, now),
        }
    }
}

/// The partial `POST /certificates/dns/all` answers with: the content of `#cert-body`, and the
/// summary flash swapped out of band into `#cert-result`, the page's persistent live region.
#[derive(Template)]
#[template(path = "cert_body.html")]
struct CertBodyPartial {
    body: CertBody,
}

/// The partial `POST /certificates/dns` answers with.
#[derive(Template)]
#[template(path = "cert_dns.html")]
pub struct DnsCell {
    dns: DnsView,
}

#[cfg(test)]
pub(super) mod tests;
