//! The values the [`Store`](super::Store) reads and writes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::logging::DEPLOY_TARGET;

/// A point in time, stored as milliseconds since the Unix epoch (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The current wall-clock time.
    pub fn now() -> Self {
        Self::from(SystemTime::now())
    }

    /// Milliseconds since the Unix epoch.
    pub fn as_millis(self) -> i64 {
        self.0
    }

    /// The time `millis` milliseconds after the Unix epoch, as [`Timestamp::as_millis`] gives
    /// it: for text that carries one, such as a page cursor.
    pub const fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// The start of the `unit`-long period since the epoch that holds this time: the time
    /// itself when it is a multiple of `unit`. A zero `unit` leaves the time as it is.
    pub fn floor(self, unit: Duration) -> Timestamp {
        match i64::try_from(unit.as_millis()).unwrap_or(i64::MAX) {
            0 => self,
            unit => Timestamp(self.0.saturating_sub(self.0.rem_euclid(unit))),
        }
    }
}

impl std::ops::Add<Duration> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: Duration) -> Timestamp {
        let millis = i64::try_from(rhs.as_millis()).unwrap_or(i64::MAX);
        Timestamp(self.0.saturating_add(millis))
    }
}

impl std::ops::Sub<Duration> for Timestamp {
    type Output = Timestamp;

    fn sub(self, rhs: Duration) -> Timestamp {
        let millis = i64::try_from(rhs.as_millis()).unwrap_or(i64::MAX);
        Timestamp(self.0.saturating_sub(millis))
    }
}

impl From<SystemTime> for Timestamp {
    fn from(time: SystemTime) -> Self {
        let millis = match time.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
            Err(before) => i64::try_from(before.duration().as_millis())
                .map(|m| -m)
                .unwrap_or(i64::MIN),
        };
        Self(millis)
    }
}

impl From<Timestamp> for SystemTime {
    fn from(ts: Timestamp) -> Self {
        let offset = Duration::from_millis(ts.0.unsigned_abs());
        if ts.0 >= 0 {
            UNIX_EPOCH + offset
        } else {
            UNIX_EPOCH - offset
        }
    }
}

/// Primary key of a stored stack revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct RevisionId(i64);

/// Ids are handed out by the store; tests elsewhere fabricate them.
#[cfg(test)]
impl From<i64> for RevisionId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl fmt::Display for RevisionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Primary key of a deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct DeploymentId(i64);

/// Ids are handed out by the store; tests elsewhere fabricate them.
#[cfg(test)]
impl From<i64> for DeploymentId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<DeploymentId> for i64 {
    fn from(id: DeploymentId) -> Self {
        id.0
    }
}

/// As [`Display`](fmt::Display) writes it, e.g. in a URL.
impl std::str::FromStr for DeploymentId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Self)
    }
}

impl fmt::Display for DeploymentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A database schema version: the version of the newest migration applied, `0` for an empty
/// database. Migration versions are the numeric prefixes of the files in `migrations/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SchemaVersion(i64);

impl From<i64> for SchemaVersion {
    fn from(version: i64) -> Self {
        Self(version)
    }
}

impl From<SchemaVersion> for i64 {
    fn from(version: SchemaVersion) -> Self {
        version.0
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// SHA-256 of a stack file's text, lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    /// Hashes `content`.
    pub fn of(content: &str) -> Self {
        let digest = Sha256::digest(content.as_bytes());
        let hex = digest.iter().fold(String::with_capacity(64), |mut s, b| {
            use fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        });
        Self(hex)
    }
}

impl AsRef<str> for ContentHash {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A deployment's state (docs/architecture.md §5.1). The transitions between them belong to the
/// orchestrator; the store only records them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, sqlx::Type)]
#[sqlx(rename_all = "snake_case")]
pub enum DeploymentState {
    Received,
    Queued,
    Noop,
    Validating,
    Rejected,
    Pulling,
    Starting,
    HealthChecking,
    Replacing,
    Switching,
    Draining,
    CleaningUp,
    Aborting,
    Failed,
    Live,
    LiveWithWarnings,
    /// A queued deployment replaced by a newer request for the same stack (ADR-0040).
    Superseded,
}

impl DeploymentState {
    /// Whether the deployment is over: a deployment in a terminal state never moves again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Noop
                | Self::Rejected
                | Self::Failed
                | Self::Live
                | Self::LiveWithWarnings
                | Self::Superseded
        )
    }

    /// Whether an active deployment in this state may still be cancelled (ADR-0040): it has
    /// not reached `Switching`, the commit point, and can abort back to how the stack was.
    pub fn is_cancellable(self) -> bool {
        matches!(
            self,
            Self::Received
                | Self::Validating
                | Self::Pulling
                | Self::Starting
                | Self::HealthChecking
                | Self::Replacing
        )
    }

    /// Whether a deployment that ended in this state can be redeployed, as a rollback
    /// (ADR-0041): it went live.
    pub fn is_redeployable(self) -> bool {
        matches!(self, Self::Live | Self::LiveWithWarnings)
    }

    /// Whether reaching this state is worth a warning in the log: the deployment did not go
    /// (entirely) as planned.
    fn is_trouble(self) -> bool {
        matches!(
            self,
            Self::Rejected | Self::Aborting | Self::Failed | Self::LiveWithWarnings
        )
    }
}

impl fmt::Display for DeploymentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// What started a deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, sqlx::Type)]
#[sqlx(rename_all = "snake_case")]
pub enum DeploymentSource {
    /// The OIDC-authenticated deploy webhook (ADR-0042, ADR-0047).
    Webhook,
    /// The registry digest poller (ADR-0043).
    Poller,
    /// A stack revision applied, or a secret changed, in the UI (ADR-0049).
    Apply,
    /// A manual deploy from the UI.
    Manual,
    /// "Redeploy this one" on an earlier deployment (ADR-0041).
    Rollback,
    /// The update checker applied a service's `auto_update` policy (ADR-0061 §10).
    AutoUpdate,
}

impl fmt::Display for DeploymentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// The source's stored name (as `#[sqlx(rename_all = "snake_case")]` writes it), which the
/// event log shows too.
impl AsRef<str> for DeploymentSource {
    fn as_ref(&self) -> &str {
        match self {
            Self::Webhook => "webhook",
            Self::Poller => "poller",
            Self::Apply => "apply",
            Self::Manual => "manual",
            Self::Rollback => "rollback",
            Self::AutoUpdate => "auto_update",
        }
    }
}

/// A digest shortened for tables and the event log: `sha256:` and the first 12 hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortDigest<'a>(pub &'a str);

impl fmt::Display for ShortDigest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (algorithm, hex) = self.0.split_once(':').unwrap_or(("", self.0));
        let short = hex.get(..12).unwrap_or(hex);
        if algorithm.is_empty() {
            f.write_str(short)
        } else {
            write!(f, "{algorithm}:{short}")
        }
    }
}

/// A commit sha shortened for the event log: its first 7 hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortSha<'a>(pub &'a str);

impl fmt::Display for ShortSha<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.get(..7).unwrap_or(self.0))
    }
}

/// Target image digests of a deployment, by service name.
pub type DigestSet = BTreeMap<String, String>;

/// A stack known to hoster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    pub name: String,
    /// The highest generation handed out to a deployment of this stack (0 = none yet).
    pub last_generation: u32,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// One applied stack file, kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackRevision {
    pub id: RevisionId,
    pub stack: String,
    /// 1, 2, 3, ... per stack.
    pub revision: u32,
    pub toml: String,
    pub hash: ContentHash,
    pub created_at: Timestamp,
}

/// What a new deployment wants to reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDeployment {
    pub stack: String,
    /// The stack revision deployed, if the deployment applies one.
    pub revision: Option<RevisionId>,
    pub source: DeploymentSource,
    pub target: DigestSet,
    /// Where a webhook deployment came from (ADR-0047); `None` for every other source.
    pub provenance: Option<Provenance>,
}

/// A recorded deployment attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    pub id: DeploymentId,
    pub stack: String,
    pub revision: Option<RevisionId>,
    /// Assigned in `Validating` (§5.1).
    pub generation: Option<u32>,
    pub state: DeploymentState,
    pub source: DeploymentSource,
    pub target: DigestSet,
    /// Where a webhook deployment came from (ADR-0047).
    pub provenance: Option<Provenance>,
    /// Why the deployment failed or was rejected, if it did.
    pub error: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// When the per-service pointers were written: the deploy's commit point (§5.4).
    pub committed_at: Option<Timestamp>,
    /// When the deployment reached a terminal state.
    pub finished_at: Option<Timestamp>,
}

impl Deployment {
    /// How long the deployment took: from its creation to its end, or to now while it runs.
    /// Zero if the clock went backwards in between.
    pub fn duration(&self) -> Duration {
        let end = self.finished_at.unwrap_or_else(Timestamp::now);
        let millis = end.as_millis().saturating_sub(self.created_at.as_millis());
        Duration::from_millis(u64::try_from(millis).unwrap_or(0))
    }
}

/// Where a webhook deployment came from: the verified OIDC token's claims (ADR-0047), plus
/// the informational `tag` of the request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// `owner/name` of the repository whose workflow sent the deploy.
    pub repository: String,
    /// The commit the workflow ran on.
    pub sha: String,
    /// The token's `ref`, e.g. `refs/tags/v1.4.2`.
    pub git_ref: String,
    /// The workflow run's id.
    pub run_id: String,
    /// The GitHub user who triggered the run.
    pub actor: String,
    /// The tag the request body names; not verified.
    pub tag: Option<String>,
}

/// A webhook attempt that carried a valid token but was refused (ADR-0047), with the token's
/// claims (never the token itself), so the admin can see who tried and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookRejection {
    pub stack: String,
    /// Why it was refused, for the admin.
    pub reason: String,
    pub repository: String,
    /// GitHub's numeric repository id, the value a `[deploy]` rule names.
    pub repository_id: String,
    pub git_ref: String,
    pub environment: Option<String>,
    pub workflow_ref: Option<String>,
    pub job_workflow_ref: Option<String>,
    pub actor: String,
    pub run_id: String,
    pub at: Timestamp,
}

/// How the finished deployments of one stack that ended in one state went, for the statistics
/// of the web UI (ADR-0045).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentOutcome {
    pub stack: String,
    /// The terminal state they reached.
    pub state: DeploymentState,
    pub count: u32,
    /// The mean time from receiving a deployment to finishing it, queueing included.
    pub mean: Duration,
    /// The longest of those times.
    pub max: Duration,
}

/// One entry of a deployment's transition log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentEvent {
    pub from: Option<DeploymentState>,
    pub to: DeploymentState,
    pub message: Option<String>,
    pub at: Timestamp,
}

impl DeploymentEvent {
    /// Writes this event to hoster's log as well (target [`DEPLOY_TARGET`]): at `warn` when the
    /// deployment ran into trouble, `info` otherwise.
    pub(super) fn log(&self, deployment_id: DeploymentId, stack: &str) {
        let from = self.from.map(|from| from.to_string());
        let detail = self.message.as_deref();
        if self.to.is_trouble() {
            tracing::warn!(
                target: DEPLOY_TARGET,
                %deployment_id, stack, from, to = %self.to, detail,
                "deployment event"
            );
        } else {
            tracing::info!(
                target: DEPLOY_TARGET,
                %deployment_id, stack, from, to = %self.to, detail,
                "deployment event"
            );
        }
    }
}

/// Loopback host ports of a container, by the container port they publish (ADR-0007, ADR-0026).
pub type PortMap = BTreeMap<u16, u16>;

/// The current pointer of one service: which generation runs, from which image and config, and
/// on which loopback ports (one per distinct container port its routes use; none for services
/// without routes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePointer {
    pub service: String,
    pub generation: u32,
    pub digest: String,
    pub config_hash: String,
    pub ports: PortMap,
}

/// An image hoster pulled, as the `images` table records it (ADR-0041): image garbage collection
/// only ever removes images recorded here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedImage {
    /// `registry/repository`.
    pub repository: String,
    pub digest: String,
    pub pulled_at: Timestamp,
}

/// Why a service's digest is wanted, for image retention (ADR-0041).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageUseKind {
    /// The service's current pointer.
    Current,
    /// The target of a deployment that went live: a rollback candidate.
    Live(DeploymentId),
    /// The target of a deployment that has not finished yet.
    InFlight,
}

/// One digest a service of a (not removed) stack uses or may roll back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageUse {
    pub stack: String,
    pub service: String,
    pub digest: String,
    pub kind: ImageUseKind,
}

/// A loopback port reserved for one container (ADR-0026).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortAllocation {
    pub port: u16,
    pub stack: String,
    pub service: String,
    pub container_port: u16,
    pub generation: u32,
}

/// The container a loopback port is wanted for: a [`PortAllocation`] before its port is chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRequest {
    pub stack: String,
    pub service: String,
    pub container_port: u16,
    pub generation: u32,
}

impl PortRequest {
    /// The allocation of `port` to this container.
    pub fn at(&self, port: u16) -> PortAllocation {
        PortAllocation {
            port,
            stack: self.stack.clone(),
            service: self.service.clone(),
            container_port: self.container_port,
            generation: self.generation,
        }
    }
}

/// A server-side admin session (ADR-0031). The cookie holds the token; the store keeps only
/// `token_hash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// SHA-256 (hex) of the cookie token.
    pub token_hash: String,
    /// The per-session CSRF token HTMX sends back in a header.
    pub csrf_token: String,
    pub github_login: String,
    pub github_id: i64,
    pub created_at: Timestamp,
    pub last_seen_at: Timestamp,
    /// The absolute expiry.
    pub expires_at: Timestamp,
}

/// One secret as the store keeps it: sealed under the secret-store key (ADR-0029). Only
/// `secrets::SecretStore` opens it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSecret {
    pub name: String,
    /// 24 random bytes, fresh for every write.
    pub nonce: Vec<u8>,
    /// The XChaCha20-Poly1305 ciphertext, tag included.
    pub ciphertext: Vec<u8>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A secret's name and when it last changed: everything the UI may show of it (ADR-0029).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretEntry {
    pub name: String,
    pub updated_at: Timestamp,
}

/// Primary key of an [`Event`] in the event log (ADR-0055).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct EventId(i64);

/// Ids are handed out by the store; tests elsewhere fabricate them.
#[cfg(test)]
impl From<i64> for EventId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What an [`Event`] reports (ADR-0055 §2). Stored as its [`Display`](fmt::Display) text, e.g.
/// `deploy_succeeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, sqlx::Type)]
#[sqlx(rename_all = "snake_case")]
pub enum EventKind {
    /// A deployment started; carries its source and, if known, the commit sha.
    DeployStarted,
    /// A deployment went live.
    DeploySucceeded,
    /// A deployment failed or was rejected.
    DeployFailed,
    /// A deployment redeployed an earlier one (ADR-0041).
    DeployRolledBack,
    /// The reconciler restarted a container (its exit reason, OOM kills named per ADR-0059 §6).
    ContainerRestarted,
    /// A service entered crash loop (ADR-0028).
    CrashLoopEntered,
    /// A service left crash loop.
    CrashLoopLeft,
    /// A live service's health changed (ADR-0038).
    HealthChanged,
    /// A certificate was issued for a host for the first time.
    CertificateIssued,
    /// A certificate was renewed.
    CertificateRenewed,
    /// Issuing or renewing a certificate failed (the reason and next retry).
    CertificateFailed,
    /// A routed host's DNS does not point at this server.
    DnsMismatch,
    /// A stack file was applied.
    StackApplied,
    /// A stack was removed.
    StackRemoved,
    /// A secret was set (its name only, never the value).
    SecretSet,
    /// A secret was deleted (its name only).
    SecretDeleted,
    /// hoster started.
    HosterStarted,
    /// hoster updated itself to a new version (ADR-0052).
    HosterUpdated,
    /// hoster rolled back to its previous version (ADR-0052).
    HosterRolledBack,
    /// A notification mail could not be delivered (ADR-0058 §9); never itself mailed.
    NotificationFailed,
    /// The update checker found an update of a third-party image (ADR-0061 §8); never mailed.
    UpdateAvailable,
}

impl EventKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 21] = [
        Self::DeployStarted,
        Self::DeploySucceeded,
        Self::DeployFailed,
        Self::DeployRolledBack,
        Self::ContainerRestarted,
        Self::CrashLoopEntered,
        Self::CrashLoopLeft,
        Self::HealthChanged,
        Self::CertificateIssued,
        Self::CertificateRenewed,
        Self::CertificateFailed,
        Self::DnsMismatch,
        Self::StackApplied,
        Self::StackRemoved,
        Self::SecretSet,
        Self::SecretDeleted,
        Self::HosterStarted,
        Self::HosterUpdated,
        Self::HosterRolledBack,
        Self::NotificationFailed,
        Self::UpdateAvailable,
    ];

    /// The text stored in the `kind` column. It must match the `#[sqlx(rename_all)]` encoding,
    /// since `event_page` filters on it; `events_are_stored_as_their_text_and_read_back` pins
    /// the two together.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeployStarted => "deploy_started",
            Self::DeploySucceeded => "deploy_succeeded",
            Self::DeployFailed => "deploy_failed",
            Self::DeployRolledBack => "deploy_rolled_back",
            Self::ContainerRestarted => "container_restarted",
            Self::CrashLoopEntered => "crash_loop_entered",
            Self::CrashLoopLeft => "crash_loop_left",
            Self::HealthChanged => "health_changed",
            Self::CertificateIssued => "certificate_issued",
            Self::CertificateRenewed => "certificate_renewed",
            Self::CertificateFailed => "certificate_failed",
            Self::DnsMismatch => "dns_mismatch",
            Self::StackApplied => "stack_applied",
            Self::StackRemoved => "stack_removed",
            Self::SecretSet => "secret_set",
            Self::SecretDeleted => "secret_deleted",
            Self::HosterStarted => "hoster_started",
            Self::HosterUpdated => "hoster_updated",
            Self::HosterRolledBack => "hoster_rolled_back",
            Self::NotificationFailed => "notification_failed",
            Self::UpdateAvailable => "update_available",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Text that names no [`EventKind`] or [`Severity`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {what} {text:?}")]
pub struct UnknownEventText {
    what: &'static str,
    text: String,
}

/// As [`Display`](fmt::Display) writes it, e.g. in a filter's query string.
impl std::str::FromStr for EventKind {
    type Err = UnknownEventText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| UnknownEventText {
                what: "event kind",
                text: s.to_owned(),
            })
    }
}

/// How an [`Event`] reads in the UI: its tone dot. Ordered from least to most severe, so a
/// filter can ask for "at least" one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, sqlx::Type)]
#[sqlx(rename_all = "snake_case")]
pub enum Severity {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

impl Severity {
    /// Every severity, least severe first.
    pub const ALL: [Self; 4] = [Self::Info, Self::Success, Self::Warning, Self::Error];

    /// The text stored in the `severity` column. It must match the `#[sqlx(rename_all)]`
    /// encoding, since `event_page` filters on it; `events_are_stored_as_their_text_and_read_back`
    /// pins the two together.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// As [`Display`](fmt::Display) writes it.
impl std::str::FromStr for Severity {
    type Err = UnknownEventText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|severity| severity.as_str() == s)
            .ok_or_else(|| UnknownEventText {
                what: "severity",
                text: s.to_owned(),
            })
    }
}

/// An event to record in the event log (ADR-0055). The message is the readable line; it must
/// never contain a secret's value (secret events carry the name only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub kind: EventKind,
    pub severity: Severity,
    /// The stack it concerns, if any.
    pub stack: Option<String>,
    /// The service, host or deployment it links to, if any.
    pub subject: Option<String>,
    pub message: String,
    /// The short mono line: sha, digest, exit code, version.
    pub meta: Option<String>,
}

impl NewEvent {
    /// An event of `kind` with no stack, subject or meta.
    pub fn new(kind: EventKind, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            kind,
            severity,
            stack: None,
            subject: None,
            message: message.into(),
            meta: None,
        }
    }

    /// The event, about `stack`.
    pub fn stack(mut self, stack: impl Into<String>) -> Self {
        self.stack = Some(stack.into());
        self
    }

    /// The event, linking to `subject`.
    pub fn subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// The event, with `meta` as its mono line.
    pub fn meta(mut self, meta: impl Into<String>) -> Self {
        self.meta = Some(meta.into());
        self
    }
}

/// A recorded event of the event log (ADR-0055).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: EventId,
    pub at: Timestamp,
    pub kind: EventKind,
    pub severity: Severity,
    pub stack: Option<String>,
    pub subject: Option<String>,
    pub message: String,
    pub meta: Option<String>,
}

impl Event {
    /// Where a page of events older than this one starts.
    pub fn cursor(&self) -> EventCursor {
        EventCursor {
            at: self.at,
            id: self.id,
        }
    }
}

#[cfg(test)]
impl Event {
    /// Whether a page that starts at `cursor` holds this event, as the store pages; for fakes.
    pub(crate) fn is_older_than(&self, cursor: EventCursor) -> bool {
        (self.at, self.id) < (cursor.at, cursor.id)
    }
}

/// A position in the event log, newest first: a page that starts at it holds only the events
/// older than the event it was taken from. Written as `<at>.<id>`, e.g. in a "more" link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventCursor {
    pub(super) at: Timestamp,
    pub(super) id: EventId,
}

impl fmt::Display for EventCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.at.0, self.id)
    }
}

/// Text that is not an [`EventCursor`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid event cursor {0:?}")]
pub struct InvalidEventCursor(String);

impl std::str::FromStr for EventCursor {
    type Err = InvalidEventCursor;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidEventCursor(s.to_owned());
        let (at, id) = s.split_once('.').ok_or_else(invalid)?;
        Ok(Self {
            at: Timestamp(at.parse().map_err(|_| invalid())?),
            id: EventId(id.parse().map_err(|_| invalid())?),
        })
    }
}

/// Which events a page of the event log shows. The default is the first page of every event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventQuery {
    /// Only the events of this stack.
    pub stack: Option<String>,
    /// Only events of these kinds; every kind when empty.
    pub kinds: BTreeSet<EventKind>,
    /// Only events at least this severe.
    pub min_severity: Severity,
    /// Only events older than this position.
    pub before: Option<EventCursor>,
    /// At most this many events; clamped to `1..=`[`EventQuery::MAX`].
    pub limit: u32,
}

impl EventQuery {
    /// How many events a page holds unless asked otherwise.
    pub const PAGE: u32 = 50;
    /// The most events one page holds, whatever `limit` asks for.
    pub const MAX: u32 = 500;
}

impl Default for EventQuery {
    fn default() -> Self {
        Self {
            stack: None,
            kinds: BTreeSet::new(),
            min_severity: Severity::Info,
            before: None,
            limit: Self::PAGE,
        }
    }
}

/// One page of the event log, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventPage {
    pub events: Vec<Event>,
    /// Where the next (older) page starts, if there are older events matching the query.
    pub next: Option<EventCursor>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_trips_through_system_time() {
        let ts = Timestamp(1_700_000_000_123);
        assert_eq!(Timestamp::from(SystemTime::from(ts)), ts);
        let before = Timestamp(-5);
        assert_eq!(Timestamp::from(SystemTime::from(before)), before);
    }

    #[test]
    fn content_hash_is_sha256_hex() {
        assert_eq!(
            ContentHash::of("").as_ref(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn event_kinds_and_severities_round_trip_through_their_text() {
        for kind in EventKind::ALL {
            assert_eq!(kind.to_string().parse::<EventKind>(), Ok(kind));
        }
        for severity in Severity::ALL {
            assert_eq!(severity.to_string().parse::<Severity>(), Ok(severity));
        }
        assert!("deploy".parse::<EventKind>().is_err());
        assert!("fatal".parse::<Severity>().is_err());
        assert!(Severity::Info < Severity::Success);
        assert!(Severity::Warning < Severity::Error);
    }

    #[test]
    fn event_cursors_round_trip_through_their_text() {
        let cursor = EventCursor {
            at: Timestamp(1_700_000_000_123),
            id: EventId(42),
        };
        assert_eq!(cursor.to_string(), "1700000000123.42");
        assert_eq!(cursor.to_string().parse(), Ok(cursor));
        for bad in ["", "12", "a.1", "1.b", "1.2.3"] {
            assert!(bad.parse::<EventCursor>().is_err(), "{bad}");
        }
    }

    #[test]
    fn terminal_states() {
        assert!(DeploymentState::Live.is_terminal());
        assert!(DeploymentState::Noop.is_terminal());
        assert!(!DeploymentState::Draining.is_terminal());
        assert!(!DeploymentState::Queued.is_terminal());
    }
}
