//! Stack file model, parsing and validation (ADR-0006, `site/content/wiki/stack-format.md`).
//!
//! Pure: no I/O beyond the input text, so it carries most of the unit tests.
//!
//! The public surface is [`Stack::parse`] (or [`str::parse`]) and the validated model it returns.
//! Parsing happens in two passes: strict serde deserialization with `deny_unknown_fields` into
//! private span-carrying types, then semantic validation that collects *every* problem instead
//! of stopping at the first. Each [`Diagnostic`] names the TOML line/column and the field path.
//!
//! The same raw types generate the stack file's JSON Schema ([`StackSchema`]) for editors, and
//! [`StackCheck`] runs the parse over files on disk for `hoster stack check`.
//!
//! Checks that need other stacks or hoster's configuration (route collisions, ADR-0012; the
//! reserved admin host) are not done here; they live with the routing table.

// The orchestrator and web UI that consume the model land in later tasks.
#![allow(dead_code)]

mod check;
mod dependency;
mod deploy;
mod diagnostic;
mod edit;
mod file;
mod image;
mod limits;
mod raw;
mod route;
mod schema;
mod size;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::otlp::ExporterEnv;

pub use check::StackCheck;
pub use deploy::{DeployAuth, DeployCaller};
pub use diagnostic::{Diagnostic, FieldPath, Location, StackError};
pub use edit::ImageEdits;
pub use file::{ConfigFile, ContainerPath, FileMode, FileOwner, FileSource, Ownership};
#[allow(
    unused_imports,
    reason = "consumed by the stack's `auto_update` check and the update checker (ADR-0061)"
)]
pub use image::{Bump, TagVersion};
pub use image::{Digest, ImageRef, PinnedImage};
pub use limits::{Cpus, Limit, Limits};
pub use route::{Host, NormalizedPrefix};
pub use schema::StackSchema;
pub use size::ByteSize;

/// Default upper bound for draining old upstreams (ADR-0017).
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Default SIGTERM → SIGKILL grace period.
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Default HTTP health check path for routed services (ADR-0051).
pub const DEFAULT_HEALTH_PATH: &str = "/healthz";
/// The only schema version this hoster understands.
pub const SUPPORTED_VERSION: i64 = 1;

/// A validated stack definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    pub name: Name,
    pub drain_timeout: Duration,
    /// Who may deploy through the webhook (ADR-0047); `None` means UI-only deploys.
    pub deploy: Option<DeployAuth>,
    /// At least one service, keyed by service name (also its DNS alias on the stack network).
    pub services: BTreeMap<Name, Service>,
}

/// The result of a successful parse: the stack plus non-fatal warnings to show the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedStack {
    pub stack: Stack,
    pub warnings: Vec<Diagnostic>,
}

impl Stack {
    /// Parses and validates a stack file. On failure, every problem found is reported.
    pub fn parse(source: &str) -> Result<ParsedStack, StackError> {
        raw::RawStack::parse(source)
    }
}

/// Parses and validates, discarding warnings. Use [`Stack::parse`] to show them.
impl FromStr for Stack {
    type Err = StackError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        Stack::parse(source).map(|parsed| parsed.stack)
    }
}

/// A stack or service name: `^[a-z][a-z0-9-]{0,30}$`. Used in podman object names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

impl FromStr for Name {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chars = s.chars();
        let valid = s.len() <= 31
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!(
                "invalid name `{s}`: must start with a lowercase letter and contain only \
                 lowercase letters, digits and `-`, at most 31 characters"
            ))
        }
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One container of a stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub image: ImageRef,
    /// Services of this stack that must pass their check before this one starts (ADR-0056).
    /// Validated: known, not itself, acyclic. Not part of the config hash; read the graph
    /// through [`Stack::dependencies`] and its siblings.
    pub depends_on: BTreeSet<Name>,
    /// Tag the ghcr poller watches (ADR-0043). Only set for CI-built ghcr images.
    pub track: Option<String>,
    /// Whether the update checker applies this image's updates by itself (ADR-0061). Anything
    /// but `Off` only on third-party images with a tag, and `Patch`/`Minor` only on a tag that
    /// reads as a version. Not part of the config hash: changing it never redeploys.
    pub auto_update: AutoUpdate,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    /// Env var name → secret name `<stack>/<name>` in the secret store (ADR-0029).
    pub secrets: BTreeMap<String, String>,
    /// Volume name → absolute mount path (ADR-0030).
    pub volumes: BTreeMap<String, String>,
    /// Config files written into each container before it starts, by container path
    /// (ADR-0057). No path is a parent directory of another, and none is in or above a volume.
    pub files: BTreeMap<ContainerPath, ConfigFile>,
    /// Declared health check. `None` means the defaults of ADR-0038/ADR-0051 apply. A service
    /// without routes only ever declares an [`Probe::Exec`] check (ADR-0056).
    pub health: Option<HealthCheck>,
    pub update: UpdateStrategy,
    pub stop_timeout: Duration,
    /// `uid[:gid]` inside the container.
    pub user: Option<String>,
    pub read_only: bool,
    /// The size of `/dev/shm`, at least 1 MiB (ADR-0059 §1). `None` keeps podman's 65536000
    /// bytes.
    pub shm_size: Option<ByteSize>,
    /// tmpfs mounts by container path, each at least 1 MiB (ADR-0059 §1). No path is in
    /// `/proc`, `/sys` or `/dev`, or at, in or above a volume, a config file or another tmpfs.
    pub tmpfs: BTreeMap<ContainerPath, ByteSize>,
    /// Run podman's init (catatonit) as PID 1 (ADR-0059 §1).
    pub init: bool,
    /// Resource limits (ADR-0059 §2); unset ones keep podman's defaults.
    pub limits: Limits,
    pub routes: Vec<Route>,
}

impl Service {
    /// Every use of a secret by this service: its environment variables first, then its
    /// config files, each in name order. A secret used twice is listed twice.
    ///
    /// This is the one place that knows where a stack file can name a secret (ADR-0057 §6):
    /// collecting versions, checking that secrets are set, redeploying on a change and
    /// redacting values all read it.
    pub fn secret_refs(&self) -> impl Iterator<Item = SecretRef<'_>> {
        let env = self.secrets.iter().map(|(name, secret)| SecretRef {
            secret,
            usage: SecretUse::Env(name),
        });
        let files = self
            .files
            .iter()
            .filter_map(|(path, file)| match &file.source {
                FileSource::Secret(secret) => Some(SecretRef {
                    secret,
                    usage: SecretUse::File(path),
                }),
                FileSource::Inline(_) => None,
            });
        env.chain(files)
    }

    /// The plain environment hoster gives the container of the service called `name`: `env`,
    /// plus `PORT=<container port>` for a routed service with exactly one distinct container
    /// port across its routes (ADR-0051), plus the four `OTEL_*` variables that point an
    /// OpenTelemetry SDK at hoster's receiver (ADR-0069 §5). Every added variable is left out
    /// when the stack file [`sets`](Self::sets) its name, as `env` or as a secret, so the
    /// container never gets a name twice. This is what the config hash covers; secrets and the
    /// two `OTEL_*` variables that change per deployment are added at container create, never
    /// here.
    pub fn container_env(&self, name: &Name) -> BTreeMap<String, String> {
        let mut env = self.env.clone();
        let ports: BTreeSet<u16> = self.routes.iter().map(|r| r.port).collect();
        let port = match ports.into_iter().collect::<Vec<_>>()[..] {
            [port] => Some(("PORT", port.to_string())),
            _ => None,
        };
        for (key, value) in port.into_iter().chain(ExporterEnv::hashed(name)) {
            if !self.sets(key) {
                env.insert(key.to_owned(), value);
            }
        }
        env
    }

    /// Whether the stack file gives the container `key` itself, as `env` or as a secret, so
    /// hoster adds no value of its own under that name.
    pub fn sets(&self, key: &str) -> bool {
        self.env.contains_key(key) || self.secrets.contains_key(key)
    }
}

/// One use of a secret by a service: the secret's name and what it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretRef<'a> {
    /// The secret's name in the store, `<stack>/<name>`.
    pub secret: &'a str,
    pub usage: SecretUse<'a>,
}

/// What a service uses a secret for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretUse<'a> {
    /// The value of this environment variable (ADR-0029).
    Env(&'a str),
    /// The content of the config file at this path (ADR-0057).
    File(&'a ContainerPath),
}

/// As messages name it after "for": `DATABASE_URL`, or `file /etc/app/credentials.json`.
impl fmt::Display for SecretUse<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretUse::Env(name) => f.write_str(name),
            SecretUse::File(path) => write!(f, "file {path}"),
        }
    }
}

/// Whether hoster applies a third-party image's updates by itself (ADR-0061 §4). Updates are
/// shown in the UI whatever the policy; majors are never applied automatically.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum AutoUpdate {
    /// Only show available updates.
    #[default]
    Off,
    /// Follow the tag to new digests (a floating tag's rebuilds and patch releases).
    Digest,
    /// Also move to newer patch versions in the same line, e.g. `2.3.1` → `2.3.4`.
    Patch,
    /// Also move to newer minor versions in the same line, e.g. `2.3.1` → `2.4.0`.
    Minor,
}

impl AutoUpdate {
    /// Whether the policy moves the tag to newer versions, which needs a tag that reads as a
    /// [`TagVersion`].
    pub fn follows_versions(self) -> bool {
        matches!(self, Self::Patch | Self::Minor)
    }

    /// Whether the policy applies an update of `kind` by itself. A major never is.
    pub fn allows(self, kind: crate::store::UpdateKind) -> bool {
        use crate::store::UpdateKind as K;
        match kind {
            K::Digest => self >= Self::Digest,
            K::Patch => self >= Self::Patch,
            K::Minor => self >= Self::Minor,
            K::Major => false,
        }
    }
}

impl fmt::Display for AutoUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Digest => "digest",
            Self::Patch => "patch",
            Self::Minor => "minor",
        })
    }
}

/// How a service is replaced on deploy (ADR-0039).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateStrategy {
    /// Start the new container and switch traffic once it is healthy: no downtime, but old and
    /// new run side by side for a moment.
    #[default]
    StartFirst,
    /// Stop the old container before starting the new one, for services that cannot run twice
    /// (e.g. a database on a volume): a short outage answered with 503 + `Retry-After`.
    StopFirst,
}

/// A (host, prefix) → container port mapping (ADR-0007, ADR-0009, ADR-0010).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub host: Host,
    pub prefix: NormalizedPrefix,
    pub strip_prefix: bool,
    /// Container port the app listens on.
    pub port: u16,
    pub timeouts: RouteTimeouts,
}

/// Per-route proxy timeouts (ADR-0011); `None` means hoster's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouteTimeouts {
    pub connect: Option<Duration>,
    pub read: Option<Duration>,
    pub write: Option<Duration>,
    pub idle: Option<Duration>,
}

/// A declared health check (ADR-0038). Unset timings mean hoster's defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    pub probe: Probe,
    pub interval: Option<Duration>,
    pub timeout: Option<Duration>,
    pub healthy_after: Option<u32>,
    pub start_period: Option<Duration>,
}

/// What a health check probes: `Http` and `Tcp` on the allocated loopback port of a container
/// port, `Exec` inside the container (ADR-0056).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Http {
        path: String,
        port: u16,
        expect_status: ExpectStatus,
    },
    Tcp {
        port: u16,
    },
    /// An argv run through podman's exec API without a shell; exit 0 passes. Non-empty, with a
    /// non-empty first element.
    Exec {
        command: Vec<String>,
    },
}

/// Which HTTP status codes count as healthy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExpectStatus {
    /// 2xx–3xx, the default (ADR-0051).
    #[default]
    SuccessOrRedirect,
    /// One status class, written `"2xx"`.
    Class(u8),
    /// One exact status, written `"204"`.
    Exact(u16),
}

impl ExpectStatus {
    pub fn matches(self, status: u16) -> bool {
        match self {
            ExpectStatus::SuccessOrRedirect => (200..400).contains(&status),
            ExpectStatus::Class(class) => status / 100 == u16::from(class),
            ExpectStatus::Exact(expected) => status == expected,
        }
    }
}

/// As a stack file writes it: `2xx`, `204`, or `2xx-3xx` for the default.
impl fmt::Display for ExpectStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpectStatus::SuccessOrRedirect => f.write_str("2xx-3xx"),
            ExpectStatus::Class(class) => write!(f, "{class}xx"),
            ExpectStatus::Exact(code) => write!(f, "{code}"),
        }
    }
}

impl FromStr for ExpectStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || format!("invalid status `{s}`: expected a class like \"2xx\" or a code");
        match s.as_bytes() {
            [d @ b'1'..=b'5', b'x', b'x'] => Ok(ExpectStatus::Class(d - b'0')),
            _ => match s.parse::<u16>() {
                Ok(code) if (100..600).contains(&code) => Ok(ExpectStatus::Exact(code)),
                _ => Err(err()),
            },
        }
    }
}

/// `n` things of a stack, with the noun in the plural unless `n` is 1: "1 service",
/// "2 routes". Shared by the CLI's and the UI's summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counted(pub usize, pub &'static str);

impl fmt::Display for Counted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(n, noun) = self;
        write!(f, "{n} {noun}{}", if *n == 1 { "" } else { "s" })
    }
}

#[cfg(test)]
mod tests;
