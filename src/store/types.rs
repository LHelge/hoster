//! The values the [`Store`](super::Store) reads and writes.

use std::collections::BTreeMap;
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
}

impl fmt::Display for DeploymentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
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
    fn terminal_states() {
        assert!(DeploymentState::Live.is_terminal());
        assert!(DeploymentState::Noop.is_terminal());
        assert!(!DeploymentState::Draining.is_terminal());
        assert!(!DeploymentState::Queued.is_terminal());
    }
}
