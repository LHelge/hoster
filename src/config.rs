//! Daemon configuration: `~/.config/hoster/hoster.toml` with `HOSTER_*` environment overrides
//! (ADR-0024). Secrets are never read from here; they live in separate files or systemd
//! credentials, reachable through [`Credentials`].
//!
//! # Sources, lowest to highest precedence
//!
//! 1. Built-in defaults (see `deploy/hoster.example.toml` for every key and its default).
//! 2. The TOML file: `--config <path>`, else `$XDG_CONFIG_HOME/hoster/hoster.toml`
//!    (`$HOME/.config/hoster/hoster.toml` when `XDG_CONFIG_HOME` is unset). It must exist.
//! 3. `HOSTER_*` environment variables. The rest of the name is the key path with `__` as the
//!    nesting separator, case-insensitive: `HOSTER_ADMIN_HOST`, `HOSTER_ACME__DIRECTORY_URL`,
//!    `HOSTER_PROXY__TIMEOUTS__READ=90s`, `HOSTER_UPSTREAM_PORTS__LAST=24999`. Unknown
//!    `HOSTER_*` variables are rejected like unknown keys in the file, so typos surface.
//!
//! `HOSTER_ADMIN_GITHUB` (the one admin's GitHub handle, ADR-0021) is required and comes from
//! the environment only; setting `admin_github` in the file is an error.
//!
//! Every problem is reported with the offending key, all at once, as a [`ConfigError`].

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use figment::providers::{Env, Format, Serialized, Toml};
use figment::value::{Dict, Map, Value};
use figment::{Figment, Metadata, Profile, Provider};
use serde::Deserialize;

use crate::logging::LogFormat;

/// Prefix of every environment override.
const ENV_PREFIX: &str = "HOSTER_";
/// Separator between nested keys in environment overrides.
const ENV_NESTING: &str = "__";
/// The admin identity key; environment-only (ADR-0021, ADR-0024).
const ADMIN_GITHUB_KEY: &str = "admin_github";
const ADMIN_GITHUB_ENV: &str = "HOSTER_ADMIN_GITHUB";
/// Let's Encrypt production. Staging is recommended for first runs.
const DEFAULT_ACME_DIRECTORY: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// hoster's validated configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    dead_code,
    reason = "fields are consumed by the subsystems as they are implemented"
)]
pub struct Config {
    /// Where hoster keeps SQLite, certificates, the ACME account and the admin socket.
    /// Defaults to `$XDG_DATA_HOME/hoster` (ADR-0023).
    pub state_dir: PathBuf,
    /// The secret-store key file (ADR-0029), generated on first start unless the systemd
    /// credential `secret_key` supplies the key. Defaults to `$XDG_CONFIG_HOME/hoster/secret.key`;
    /// it must not be inside `state_dir`, next to the database it unlocks.
    pub secret_key_file: PathBuf,
    /// The reserved admin host serving the web UI and the deploy webhook (ADR-0031).
    pub admin_host: Hostname,
    /// The admin's GitHub handle, from `HOSTER_ADMIN_GITHUB` only.
    pub admin_github: GithubHandle,
    /// The rootless podman API socket (ADR-0002, ADR-0023). Unset means the hoster user's own
    /// socket: `$XDG_RUNTIME_DIR/podman/podman.sock`, then `/run/user/$UID/podman/podman.sock`
    /// (never a rootful one; see `runtime::PodmanSocket`).
    #[serde(default)]
    pub podman_socket: Option<PathBuf>,
    #[serde(default)]
    pub acme: AcmeConfig,
    /// Loopback ports handed to routed services (ADR-0026).
    #[serde(default)]
    pub upstream_ports: PortRange,
    #[serde(default)]
    pub proxy: ProxyConfig,
    #[serde(default)]
    pub poller: PollerConfig,
    #[serde(default)]
    pub webhook: WebhookConfig,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub log: LogConfig,
    /// Secret sources; never part of the file.
    #[serde(skip)]
    pub credentials: Credentials,
    /// Where the values came from: the file and the `HOSTER_*` keys the environment set.
    #[serde(skip)]
    pub source: ConfigSource,
}

/// Where a loaded [`Config`] came from, for showing the effective configuration (the admin UI's
/// Settings). It holds the file's path and the names of the keys the environment overrides,
/// never their values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigSource {
    /// The `hoster.toml` that was read.
    pub file: PathBuf,
    /// The dotted keys (`acme.directory_url`) set by `HOSTER_*` variables, sorted.
    env: std::collections::BTreeSet<String>,
}

impl ConfigSource {
    /// A source reading `file`, with the dotted `env` keys overridden by the environment.
    pub fn new<I, K>(file: impl Into<PathBuf>, env: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: Into<String>,
    {
        Self {
            file: file.into(),
            env: env.into_iter().map(Into::into).collect(),
        }
    }

    /// The environment variable that sets the dotted `key`: `acme.directory_url` →
    /// `HOSTER_ACME__DIRECTORY_URL`.
    pub fn env_var(key: &str) -> String {
        format!(
            "{ENV_PREFIX}{}",
            key.to_ascii_uppercase().replace('.', ENV_NESTING)
        )
    }

    /// The variables that set `key` or any key inside it (`proxy.timeouts` also matches
    /// `proxy.timeouts.read`), sorted; empty when the value comes from the file or a default.
    pub fn overrides(&self, key: &str) -> Vec<String> {
        self.env
            .iter()
            .filter(|set| {
                set.as_str() == key
                    || set
                        .strip_prefix(key)
                        .is_some_and(|rest| rest.starts_with('.'))
            })
            .map(|set| Self::env_var(set))
            .collect()
    }

    /// Every `HOSTER_*` variable that set a key, sorted by key.
    pub fn env_vars(&self) -> Vec<String> {
        self.env.iter().map(|key| Self::env_var(key)).collect()
    }
}

/// ACME account settings (ADR-0015, ADR-0016, ADR-0051).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(
    dead_code,
    reason = "fields are consumed by the subsystems as they are implemented"
)]
pub struct AcmeConfig {
    /// ACME directory URL; Let's Encrypt production by default.
    pub directory_url: String,
    /// Contact e-mail address registered with the ACME account, if any.
    pub contact: Option<String>,
    /// hoster's public addresses for the advisory DNS check. Empty means "resolve the admin
    /// host's A/AAAA records" (ADR-0051).
    pub public_addresses: Vec<IpAddr>,
    /// Extra root certificate (PEM) to trust for the ACME directory instead of the platform
    /// roots, for test CAs such as Pebble. Unset for Let's Encrypt.
    pub root_certificate: Option<PathBuf>,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            directory_url: DEFAULT_ACME_DIRECTORY.to_owned(),
            contact: None,
            public_addresses: Vec::new(),
            root_certificate: None,
        }
    }
}

/// Embedded Pingora settings (ADR-0008, ADR-0011).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(
    dead_code,
    reason = "fields are consumed by the subsystems as they are implemented"
)]
pub struct ProxyConfig {
    /// Where the plain-HTTP (`:80`) listener binds: ACME challenges and redirects to HTTPS.
    pub http_listen: SocketAddr,
    /// Where the TLS (`:443`) listener binds; this one proxies to the routes.
    pub https_listen: SocketAddr,
    /// Worker threads per Pingora service; defaults to the number of CPUs.
    pub threads: NonZeroUsize,
    /// How long Pingora keeps serving in-flight requests after it stops accepting.
    #[serde(with = "humantime_serde")]
    pub grace_period: Duration,
    /// How long Pingora waits for its runtimes to shut down after the grace period.
    #[serde(with = "humantime_serde")]
    pub graceful_shutdown_timeout: Duration,
    /// Upstream timeouts for routes that set none of their own.
    pub timeouts: Timeouts,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            http_listen: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 80)),
            https_listen: SocketAddr::from((Ipv6Addr::UNSPECIFIED, 443)),
            threads: std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN),
            grace_period: Duration::from_secs(10),
            graceful_shutdown_timeout: Duration::from_secs(5),
            timeouts: Timeouts::default(),
        }
    }
}

/// Per-upstream timeouts (ADR-0011).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(
    dead_code,
    reason = "fields are consumed by the subsystems as they are implemented"
)]
pub struct Timeouts {
    #[serde(with = "humantime_serde")]
    pub connect: Duration,
    #[serde(with = "humantime_serde")]
    pub read: Duration,
    #[serde(with = "humantime_serde")]
    pub write: Duration,
    #[serde(with = "humantime_serde")]
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            read: Duration::from_secs(60),
            write: Duration::from_secs(60),
            idle: Duration::from_secs(90),
        }
    }
}

/// Digest poller settings (ADR-0043).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PollerConfig {
    /// Time between polls of tracked tags (jitter is added on top).
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5 * 60),
        }
    }
}

/// Deploy webhook settings (ADR-0047).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebhookConfig {
    /// Expected OIDC `aud` claim; `https://<admin_host>` when unset.
    audience: Option<String>,
}

/// hoster's own log output (ADR-0044); verbosity is `RUST_LOG`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    /// `text` (default) or `json` lines on stderr.
    pub format: LogFormat,
}

/// Admin web app settings (ADR-0021, ADR-0031).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebConfig {
    /// Serve the admin app on this loopback TCP address instead of the Unix socket in the state
    /// directory. For local development only; must be a loopback address.
    pub dev_listen: Option<SocketAddr>,
    /// File holding the GitHub OAuth App's client id. Unset: the `github_client_id` systemd
    /// credential.
    pub github_client_id_file: Option<PathBuf>,
    /// 0600 file holding the GitHub OAuth App's client secret. Unset: the
    /// `github_client_secret` systemd credential.
    pub github_client_secret_file: Option<PathBuf>,
    /// An admin session ends after this long without a request (and 12 h after login at the
    /// latest).
    #[serde(with = "humantime_serde")]
    pub session_idle_timeout: Duration,
}

impl WebConfig {
    /// The absolute session lifetime (ADR-0031); the idle timeout may not exceed it.
    pub const SESSION_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            dev_listen: None,
            github_client_id_file: None,
            github_client_secret_file: None,
            session_idle_timeout: Duration::from_secs(60 * 60),
        }
    }
}

impl Config {
    /// Loads and validates the configuration from the file at `path` (or the default location),
    /// defaults and the process environment.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let (path, figment) = Self::layers(path)?;
        let mut problems = Vec::new();
        if Figment::from(Toml::file_exact(&path)).contains(ADMIN_GITHUB_KEY) {
            problems.push(Problem::new(
                format!("{ADMIN_GITHUB_KEY} in {}", path.display()),
                format!("not allowed in the file; set {ADMIN_GITHUB_ENV} in the environment"),
            ));
        }

        // A file that cannot be read or parsed hides every key; report only that, rather than
        // claiming below that keys set in the environment are missing.
        if let Err(errors) = Provider::data(&figment) {
            problems.extend(errors.into_iter().map(|error| match &error.metadata {
                Some(_) => Problem::from(error),
                // Provider errors carry no metadata; the file is the only provider that fails.
                None => Problem::new(path.display().to_string(), error.kind.to_string()),
            }));
            return Err(ConfigError::Invalid(Problems(problems)));
        }

        if !figment.contains(ADMIN_GITHUB_KEY) {
            problems.push(Problem::new(
                ADMIN_GITHUB_ENV,
                "required environment variable is not set (the admin's GitHub handle)",
            ));
        }
        if !figment.contains("state_dir") {
            problems.push(Problem::new(
                "state_dir",
                "not set, and neither $XDG_DATA_HOME nor $HOME is set to derive it from",
            ));
        }
        if !figment.contains("secret_key_file") {
            problems.push(Problem::new(
                "secret_key_file",
                "not set, and neither $XDG_CONFIG_HOME nor $HOME is set to derive it from",
            ));
        }

        let config = match figment.extract::<Config>() {
            Ok(config) => Some(config),
            Err(errors) => {
                problems.extend(
                    errors
                        .into_iter()
                        // Already reported above with a better message.
                        .filter(|e| {
                            !is_missing(e, ADMIN_GITHUB_KEY)
                                && !is_missing(e, "state_dir")
                                && !is_missing(e, "secret_key_file")
                        })
                        .map(Problem::from),
                );
                None
            }
        };
        if let Some(config) = &config {
            problems.extend(config.validate());
        }
        match config {
            Some(mut config) if problems.is_empty() => {
                config.credentials = Credentials::from_env();
                config.source = ConfigSource::new(path, EnvOverrides::new().keys());
                Ok(config)
            }
            _ => Err(ConfigError::Invalid(Problems(problems))),
        }
    }

    /// Only `state_dir`, resolved exactly as [`load`](Self::load) resolves it (its default, the
    /// file at `path` or the default location, and `HOSTER_STATE_DIR`), without requiring or
    /// validating anything else. `hoster update` and `hoster rollback` run in an SSH session as
    /// the hoster user, which lacks the service's environment: `HOSTER_ADMIN_GITHUB` lives in the
    /// unit's drop-in (ADR-0052).
    pub fn load_state_dir(path: Option<&Path>) -> Result<PathBuf, ConfigError> {
        let (path, figment) = Self::layers(path)?;
        let problems = |errors: figment::Error| {
            ConfigError::Invalid(Problems(
                errors
                    .into_iter()
                    .map(|error| match &error.metadata {
                        Some(_) => Problem::from(error),
                        None => Problem::new(path.display().to_string(), error.kind.to_string()),
                    })
                    .collect(),
            ))
        };
        if !figment.contains("state_dir") {
            return Err(ConfigError::Invalid(Problems(vec![Problem::new(
                "state_dir",
                "not set, and neither $XDG_DATA_HOME nor $HOME is set to derive it from",
            )])));
        }
        let state_dir: PathBuf = figment.extract_inner("state_dir").map_err(problems)?;
        if !state_dir.is_absolute() {
            return Err(ConfigError::Invalid(Problems(vec![Problem::new(
                "state_dir",
                "must be an absolute path",
            )])));
        }
        Ok(state_dir)
    }

    /// The configuration file (at `path` or the default location) and the layers every value is
    /// read from: the XDG defaults, the file and the `HOSTER_*` environment.
    fn layers(path: Option<&Path>) -> Result<(PathBuf, Figment), ConfigError> {
        let dirs = XdgDirs::from_env();
        let path = match path {
            Some(path) => path.to_owned(),
            None => dirs.config_file().ok_or(ConfigError::NoConfigDir)?,
        };
        if !path.is_file() {
            return Err(ConfigError::NotFound(path));
        }
        let mut figment = Figment::new();
        if let Some(state_dir) = dirs.state_dir() {
            figment = figment.merge(Serialized::default("state_dir", state_dir));
        }
        if let Some(key_file) = dirs.secret_key_file() {
            figment = figment.merge(Serialized::default("secret_key_file", key_file));
        }
        let figment = figment
            .merge(Toml::file_exact(&path))
            .merge(EnvOverrides::new());
        Ok((path, figment))
    }

    /// The `aud` claim deploy webhook tokens must carry (ADR-0047).
    #[allow(dead_code, reason = "consumed by the webhook endpoint")]
    pub fn webhook_audience(&self) -> String {
        self.webhook
            .audience
            .clone()
            .unwrap_or_else(|| format!("https://{}", self.admin_host))
    }

    /// The origin browsers use for the admin app: `https://<admin_host>`, or
    /// `http://<web.dev_listen>` in local development. OAuth callbacks and the CSRF origin
    /// check use it.
    pub fn admin_origin(&self) -> String {
        match self.web.dev_listen {
            Some(addr) => format!("http://{addr}"),
            None => format!("https://{}", self.admin_host),
        }
    }

    /// The file name, in the state directory, of the 0600 Unix socket the admin app listens on
    /// and the proxy routes the admin host to (ADR-0031). Unused when `web.dev_listen` puts the
    /// app on loopback TCP instead.
    pub const ADMIN_SOCKET_NAME: &str = "admin.sock";

    /// Checks the rules serde cannot express; the types already enforce the rest.
    fn validate(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        if !self.state_dir.is_absolute() {
            problems.push(Problem::new("state_dir", "must be an absolute path"));
        }
        if !self.secret_key_file.is_absolute() {
            problems.push(Problem::new("secret_key_file", "must be an absolute path"));
        } else if self.secret_key_file.starts_with(&self.state_dir) {
            problems.push(Problem::new(
                "secret_key_file",
                "must not be inside state_dir: the key would sit next to the database it unlocks \
                 (ADR-0029)",
            ));
        }
        if let Some(socket) = &self.podman_socket
            && !socket.is_absolute()
        {
            problems.push(Problem::new("podman_socket", "must be an absolute path"));
        }
        if !self.acme.directory_url.starts_with("https://") {
            problems.push(Problem::new(
                "acme.directory_url",
                "must be an https:// URL",
            ));
        }
        if let Some(root) = &self.acme.root_certificate
            && !root.is_absolute()
        {
            problems.push(Problem::new(
                "acme.root_certificate",
                "must be an absolute path",
            ));
        }
        if let Some(contact) = &self.acme.contact {
            let valid = contact
                .split_once('@')
                .is_some_and(|(user, domain)| !user.is_empty() && domain.contains('.'))
                && !contact.chars().any(char::is_whitespace)
                && !contact.contains(':');
            if !valid {
                problems.push(Problem::new(
                    "acme.contact",
                    "must be a plain e-mail address such as admin@example.com",
                ));
            }
        }
        let durations = [
            ("proxy.grace_period", self.proxy.grace_period),
            (
                "proxy.graceful_shutdown_timeout",
                self.proxy.graceful_shutdown_timeout,
            ),
            ("proxy.timeouts.connect", self.proxy.timeouts.connect),
            ("proxy.timeouts.read", self.proxy.timeouts.read),
            ("proxy.timeouts.write", self.proxy.timeouts.write),
            ("proxy.timeouts.idle", self.proxy.timeouts.idle),
            ("poller.interval", self.poller.interval),
        ];
        for (key, duration) in durations {
            if duration.is_zero() {
                problems.push(Problem::new(key, "must be greater than zero"));
            }
        }
        // Pingora takes these as whole seconds (`grace_period_seconds`,
        // `graceful_shutdown_timeout_seconds`); "1500ms" would silently become 1s.
        for (key, duration) in [
            ("proxy.grace_period", self.proxy.grace_period),
            (
                "proxy.graceful_shutdown_timeout",
                self.proxy.graceful_shutdown_timeout,
            ),
        ] {
            if duration.subsec_nanos() != 0 {
                problems.push(Problem::new(key, "must be a whole number of seconds"));
            }
        }
        // Port 0 would bind a random port that nothing (not even the readiness check) can find.
        for (key, addr) in [
            ("proxy.http_listen", self.proxy.http_listen),
            ("proxy.https_listen", self.proxy.https_listen),
        ] {
            if addr.port() == 0 {
                problems.push(Problem::new(key, "must name a port"));
            }
        }
        if self.proxy.http_listen == self.proxy.https_listen {
            problems.push(Problem::new(
                "proxy.https_listen",
                "must differ from proxy.http_listen",
            ));
        }
        if let Some(audience) = &self.webhook.audience
            && audience.trim().is_empty()
        {
            problems.push(Problem::new("webhook.audience", "must not be empty"));
        }
        if let Some(addr) = self.web.dev_listen
            && !addr.ip().is_loopback()
        {
            problems.push(Problem::new(
                "web.dev_listen",
                "must be a loopback address such as 127.0.0.1:8080 (development only)",
            ));
        }
        for (key, file) in [
            ("web.github_client_id_file", &self.web.github_client_id_file),
            (
                "web.github_client_secret_file",
                &self.web.github_client_secret_file,
            ),
        ] {
            if let Some(file) = file
                && !file.is_absolute()
            {
                problems.push(Problem::new(key, "must be an absolute path"));
            }
        }
        let idle = self.web.session_idle_timeout;
        if idle.is_zero() || idle > WebConfig::SESSION_LIFETIME {
            problems.push(Problem::new(
                "web.session_idle_timeout",
                "must be greater than zero and at most 12h (the absolute session lifetime)",
            ));
        }
        problems
    }
}

/// Whether `error` is serde's "missing field" for the top-level `key`.
fn is_missing(error: &figment::Error, key: &str) -> bool {
    matches!(&error.kind, figment::error::Kind::MissingField(field) if field == key)
        && error.path.len() <= 1
}

/// The `HOSTER_*` environment overrides.
///
/// figment's [`Env`] parses every value like a TOML literal, so `HOSTER_ADMIN_GITHUB=1234`
/// would arrive as an integer (and `007` as `7`), and a string field would reject it.
/// Keys whose type is a string keep the variable's text unchanged; all others are parsed as
/// [`Env`] does, so numbers, arrays and durations keep working.
struct EnvOverrides(Env);

impl EnvOverrides {
    /// Every key (after `__` → `.`) whose value is a string, path or hostname.
    const STRING_KEYS: &[&str] = &[
        "state_dir",
        "secret_key_file",
        "admin_host",
        "podman_socket",
        ADMIN_GITHUB_KEY,
        "acme.directory_url",
        "acme.contact",
        "acme.root_certificate",
        "webhook.audience",
        "web.dev_listen",
        "web.github_client_id_file",
        "web.github_client_secret_file",
        "log.format",
    ];

    fn new() -> Self {
        Self(Env::prefixed(ENV_PREFIX).split(ENV_NESTING))
    }

    /// The dotted keys the environment sets (names only).
    fn keys(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|(key, _)| key.as_str().to_owned())
            .collect()
    }

    /// Inserts `value` at the dotted `path`, creating (or replacing non-table) parents.
    fn insert(dict: &mut Dict, path: &str, value: Value) {
        match path.split_once('.') {
            None => {
                dict.insert(path.to_owned(), value);
            }
            Some((head, rest)) => {
                let entry = dict
                    .entry(head.to_owned())
                    .or_insert_with(|| Value::from(Dict::new()));
                if !matches!(entry, Value::Dict(..)) {
                    *entry = Value::from(Dict::new());
                }
                if let Value::Dict(_, inner) = entry {
                    Self::insert(inner, rest, value);
                }
            }
        }
    }
}

impl Provider for EnvOverrides {
    fn metadata(&self) -> Metadata {
        // Keeps the "environment variable(s)" name `Problem` uses to report variable names.
        self.0.metadata()
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut dict = Dict::new();
        for (key, raw) in self.0.iter() {
            let value = if Self::STRING_KEYS.contains(&key.as_str()) {
                Value::from(raw)
            } else {
                let Ok(value) = raw.parse::<Value>();
                value
            };
            Self::insert(&mut dict, key.as_str(), value);
        }
        Ok(Profile::Default.collect(dict))
    }
}

/// Why the configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "cannot locate hoster.toml: neither $XDG_CONFIG_HOME nor $HOME is set; pass --config <path>"
    )]
    NoConfigDir,
    #[error(
        "configuration file {} not found; create it (see deploy/hoster.example.toml) or pass --config <path>",
        .0.display()
    )]
    NotFound(PathBuf),
    #[error("invalid configuration:\n{0}")]
    Invalid(Problems),
    #[error("cannot read credential {name:?}")]
    Credential {
        name: String,
        #[source]
        source: std::io::Error,
    },
}

/// One problem with one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// The offending key: a dotted path, or an environment variable name.
    pub key: String,
    pub reason: String,
}

impl Problem {
    fn new(key: impl Into<String>, reason: impl Into<String>) -> Self {
        let mut reason = reason.into();
        reason.truncate(reason.trim_end().len());
        Self {
            key: key.into(),
            reason,
        }
    }
}

impl From<figment::Error> for Problem {
    fn from(error: figment::Error) -> Self {
        let dotted = error.path.join(".");
        let key = match (&error.metadata, &error.profile) {
            // A file: the dotted key and where it came from.
            (Some(md), _) if md.source.is_some() => match &md.source {
                Some(source) if dotted.is_empty() => source.to_string(),
                Some(source) => format!("{dotted} in {source}"),
                None => unreachable!(),
            },
            // The environment: the variable's name.
            (Some(md), _) if !dotted.is_empty() && md.name.contains("environment") => {
                format!(
                    "{ENV_PREFIX}{}",
                    error.path.join(ENV_NESTING).to_ascii_uppercase()
                )
            }
            _ if dotted.is_empty() => "(top level)".to_owned(),
            _ => dotted,
        };
        Self::new(key, error.kind.to_string())
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "  {}: {}", self.key, self.reason)
    }
}

/// Every problem found, one per line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problems(pub Vec<Problem>);

impl fmt::Display for Problems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, problem) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{problem}")?;
        }
        Ok(())
    }
}

/// The XDG base directories hoster derives its default paths from (ADR-0023, ADR-0024), and the
/// root of the binary layout (ADR-0052).
#[derive(Debug, Clone, Default)]
pub(crate) struct XdgDirs {
    config_home: Option<PathBuf>,
    data_home: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl XdgDirs {
    pub(crate) fn from_env() -> Self {
        let var = |name| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = var("HOME");
        Self {
            config_home: var("XDG_CONFIG_HOME")
                .or_else(|| home.as_ref().map(|h| h.join(".config"))),
            data_home: var("XDG_DATA_HOME")
                .or_else(|| home.as_ref().map(|h| h.join(".local/share"))),
            home,
        }
    }

    /// `$HOME/.local/lib/hoster`, the root of the versioned binary layout (ADR-0052). XDG has no
    /// variable for it; the unit's `%h/.local/lib/hoster` is relative to the home directory too.
    pub(crate) fn binary_root(&self) -> Option<PathBuf> {
        self.home
            .as_ref()
            .map(|home| home.join(".local/lib/hoster"))
    }

    fn config_file(&self) -> Option<PathBuf> {
        self.config_home
            .as_ref()
            .map(|dir| dir.join("hoster").join("hoster.toml"))
    }

    fn state_dir(&self) -> Option<PathBuf> {
        self.data_home.as_ref().map(|dir| dir.join("hoster"))
    }

    fn secret_key_file(&self) -> Option<PathBuf> {
        self.config_home
            .as_ref()
            .map(|dir| dir.join("hoster").join("secret.key"))
    }
}

/// Secret sources kept outside the configuration (ADR-0024): systemd credentials in
/// `$CREDENTIALS_DIRECTORY` (`LoadCredential=` / `LoadCredentialEncrypted=` in the unit).
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    dir: Option<PathBuf>,
}

impl Credentials {
    /// The credentials in `dir`, as if `$CREDENTIALS_DIRECTORY` named it.
    #[cfg(test)]
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
        }
    }

    fn from_env() -> Self {
        Self {
            dir: std::env::var_os("CREDENTIALS_DIRECTORY")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        }
    }

    /// Reads the credential `name`, without a trailing newline. `None` when there is no
    /// credentials directory or it holds no such credential.
    pub fn read(&self, name: &str) -> Result<Option<String>, ConfigError> {
        let Some(dir) = &self.dir else {
            return Ok(None);
        };
        let credential_error = |source| ConfigError::Credential {
            name: name.to_owned(),
            source,
        };
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return Err(credential_error(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "credential names are plain file names",
            )));
        }
        match std::fs::read_to_string(dir.join(name)) {
            Ok(value) => Ok(Some(value.trim_end_matches(['\n', '\r']).to_owned())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(credential_error(e)),
        }
    }
}

impl Credentials {
    /// Reads a secret from `file` when it is configured, else the credential `name`. `None`
    /// when neither holds it. A configured file must exist and must not be accessible to group
    /// or others (it is meant to be 0600).
    pub fn read_or_file(
        &self,
        name: &str,
        file: Option<&Path>,
    ) -> Result<Option<String>, ConfigError> {
        use std::os::unix::fs::PermissionsExt as _;

        let Some(file) = file else {
            return self.read(name);
        };
        let credential_error = |source| ConfigError::Credential {
            name: file.display().to_string(),
            source,
        };
        let mode = std::fs::metadata(file)
            .map_err(credential_error)?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(credential_error(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "mode {:o} lets other users read it; chmod 600 it",
                    mode & 0o777
                ),
            )));
        }
        let value = std::fs::read_to_string(file).map_err(credential_error)?;
        Ok(Some(value.trim_end_matches(['\n', '\r']).to_owned()))
    }
}

/// A lowercase fully qualified domain name, without a trailing dot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Hostname(String);

impl FromStr for Hostname {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.len() > 253 {
            return Err(format!("{s:?} must be 1 to 253 characters long"));
        }
        if s.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(format!("{s:?} must be lowercase"));
        }
        let labels: Vec<&str> = s.split('.').collect();
        if labels.len() < 2 {
            return Err(format!(
                "{s:?} is not a fully qualified domain name (e.g. hoster.example.com)"
            ));
        }
        for label in &labels {
            let valid = (1..=63).contains(&label.len())
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-');
            if !valid {
                return Err(format!(
                    "{s:?} is not a valid domain name: label {label:?} must be 1 to 63 of a-z, 0-9 and '-', not starting or ending with '-'"
                ));
            }
        }
        if labels
            .last()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(format!("{s:?} is an IP address, not a domain name"));
        }
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for Hostname {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl AsRef<str> for Hostname {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A GitHub login handle. Compare it case-insensitively (ADR-0031).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct GithubHandle(String);

impl GithubHandle {
    /// Whether `login` names the same account; GitHub handles are case-insensitive.
    pub fn matches(&self, login: &str) -> bool {
        self.0.eq_ignore_ascii_case(login)
    }
}

impl FromStr for GithubHandle {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = (1..=39).contains(&s.len())
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !s.starts_with('-')
            && !s.ends_with('-')
            && !s.contains("--");
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(format!(
                "{s:?} is not a GitHub handle (1 to 39 letters, digits or single hyphens)"
            ))
        }
    }
}

impl TryFrom<String> for GithubHandle {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl AsRef<str> for GithubHandle {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GithubHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An inclusive range of unprivileged ports with at least [`PortRange::MIN_PORTS`] ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawPortRange")]
pub struct PortRange {
    first: u16,
    last: u16,
}

impl PortRange {
    /// Lowest port hoster may allocate.
    pub const MIN_PORT: u16 = 1024;
    /// Fewest ports a range may contain: enough for several start-first deploys at once.
    pub const MIN_PORTS: usize = 16;

    #[allow(dead_code, reason = "used by the port allocator")]
    pub fn first(&self) -> u16 {
        self.first
    }

    #[allow(dead_code, reason = "used by the port allocator")]
    pub fn last(&self) -> u16 {
        self.last
    }

    /// Number of ports in the range.
    pub fn len(&self) -> usize {
        usize::from(self.last - self.first) + 1
    }

    #[allow(dead_code, reason = "used by the port allocator")]
    pub fn contains(&self, port: u16) -> bool {
        (self.first..=self.last).contains(&port)
    }
}

impl Default for PortRange {
    fn default() -> Self {
        Self {
            first: 20000,
            last: 29999,
        }
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

/// `upstream_ports` as written: `{ first = 20000, last = 29999 }`. An end that is not given
/// keeps its default, so `HOSTER_UPSTREAM_PORTS__LAST` alone overrides just that end.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawPortRange {
    first: u16,
    last: u16,
}

impl Default for RawPortRange {
    fn default() -> Self {
        let PortRange { first, last } = PortRange::default();
        Self { first, last }
    }
}

impl TryFrom<RawPortRange> for PortRange {
    type Error = String;

    fn try_from(raw: RawPortRange) -> Result<Self, Self::Error> {
        let RawPortRange { first, last } = raw;
        Self::try_from(first..=last)
    }
}

impl TryFrom<RangeInclusive<u16>> for PortRange {
    type Error = String;

    fn try_from(ports: RangeInclusive<u16>) -> Result<Self, Self::Error> {
        let (first, last) = ports.into_inner();
        if first < Self::MIN_PORT {
            return Err(format!(
                "first port {first} is privileged; the range must lie within {}-65535",
                Self::MIN_PORT
            ));
        }
        if first > last {
            return Err(format!("first port {first} is above last port {last}"));
        }
        let range = Self { first, last };
        if range.len() < Self::MIN_PORTS {
            return Err(format!(
                "{range} holds {} ports; at least {} are required",
                range.len(),
                Self::MIN_PORTS
            ));
        }
        Ok(range)
    }
}

#[cfg(test)]
#[allow(
    clippy::result_large_err,
    reason = "figment::Jail closures return figment::Error"
)]
mod tests {
    use super::*;
    use figment::Jail;
    use std::os::unix::fs::PermissionsExt as _;

    const EXAMPLE: &str = include_str!("../deploy/hoster.example.toml");
    const MINIMAL: &str = "admin_host = \"hoster.example.com\"\n";

    /// Writes `toml` into the jail, sets a sane base environment and loads it.
    fn load(jail: &mut Jail, toml: &str) -> Result<Config, ConfigError> {
        jail.create_file("hoster.toml", toml).expect("write config");
        jail.set_env("HOME", jail.directory().to_owned().display());
        jail.set_env("XDG_DATA_HOME", jail.directory().join("data").display());
        let path = jail.directory().join("hoster.toml");
        Config::load(Some(&path))
    }

    fn with_admin(jail: &mut Jail) {
        jail.set_env(ADMIN_GITHUB_ENV, "octocat");
    }

    /// The problems of an `Invalid` error.
    fn invalid(result: Result<Config, ConfigError>) -> Vec<Problem> {
        match result {
            Err(ConfigError::Invalid(Problems(problems))) => problems,
            Err(other) => panic!("expected Invalid, got {other}"),
            Ok(config) => panic!("expected an error, got {config:?}"),
        }
    }

    fn has_problem(problems: &[Problem], key: &str) -> bool {
        problems.iter().any(|p| p.key.starts_with(key))
    }

    #[test]
    fn example_config_parses() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let config = load(jail, EXAMPLE).expect("example config is valid");
            assert_eq!(config.admin_host.as_ref(), "hoster.example.com");
            assert_eq!(config.admin_github.as_ref(), "octocat");
            assert_eq!(config.upstream_ports, PortRange::default());
            assert_eq!(config.proxy.grace_period, Duration::from_secs(10));
            assert_eq!(config.proxy.http_listen.to_string(), "0.0.0.0:80");
            assert_eq!(config.proxy.https_listen.to_string(), "[::]:443");
            assert_eq!(config.proxy.timeouts, Timeouts::default());
            assert_eq!(config.poller.interval, Duration::from_secs(300));
            assert_eq!(config.acme.contact.as_deref(), Some("admin@example.com"));
            Ok(())
        });
    }

    #[test]
    fn minimal_config_uses_defaults() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let config = load(jail, MINIMAL).expect("minimal config is valid");
            assert_eq!(config.state_dir, jail.directory().join("data/hoster"));
            assert_eq!(config.acme.directory_url, DEFAULT_ACME_DIRECTORY);
            assert_eq!(config.upstream_ports.to_string(), "20000-29999");
            assert_eq!(config.upstream_ports.len(), 10000);
            assert_eq!(config.webhook_audience(), "https://hoster.example.com");
            Ok(())
        });
    }

    #[test]
    fn secret_key_file_defaults_to_the_config_dir_and_stays_out_of_state_dir() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            jail.set_env("XDG_CONFIG_HOME", jail.directory().join("cfg").display());
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(
                config.secret_key_file,
                jail.directory().join("cfg/hoster/secret.key")
            );
            let inside = format!(
                "{MINIMAL}secret_key_file = \"{}\"\n",
                jail.directory().join("data/hoster/secret.key").display()
            );
            assert!(has_problem(
                &invalid(load(jail, &inside)),
                "secret_key_file"
            ));
            assert!(has_problem(
                &invalid(load(jail, &format!("{MINIMAL}secret_key_file = \"key\"\n"))),
                "secret_key_file"
            ));
            Ok(())
        });
    }

    #[test]
    fn state_dir_falls_back_to_home() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            jail.create_file("hoster.toml", MINIMAL)?;
            jail.set_env("HOME", jail.directory().to_owned().display());
            jail.set_env("XDG_DATA_HOME", "");
            let config = Config::load(Some(&jail.directory().join("hoster.toml")))
                .map_err(|e| e.to_string())?;
            assert_eq!(
                config.state_dir,
                jail.directory().join(".local/share/hoster")
            );
            Ok(())
        });
    }

    #[test]
    fn state_dir_alone_loads_without_the_admin_handle() {
        Jail::expect_with(|jail| {
            // An SSH session as hoster: no HOSTER_ADMIN_GITHUB, which only the unit sets.
            jail.set_env("HOME", jail.directory().to_owned().display());
            jail.set_env("XDG_DATA_HOME", jail.directory().join("data").display());
            let path = jail.directory().join("hoster.toml");
            jail.create_file("hoster.toml", MINIMAL)?;
            assert!(Config::load(Some(&path)).is_err());
            assert_eq!(
                Config::load_state_dir(Some(&path)).map_err(|e| e.to_string())?,
                jail.directory().join("data/hoster")
            );

            // The file and the environment override the default, as for the service.
            jail.create_file("hoster.toml", &format!("{MINIMAL}state_dir = \"/srv/h\"\n"))?;
            assert_eq!(
                Config::load_state_dir(Some(&path)).map_err(|e| e.to_string())?,
                Path::new("/srv/h")
            );
            jail.set_env("HOSTER_STATE_DIR", "/srv/env");
            assert_eq!(
                Config::load_state_dir(Some(&path)).map_err(|e| e.to_string())?,
                Path::new("/srv/env")
            );

            jail.set_env("HOSTER_STATE_DIR", "relative");
            let err = Config::load_state_dir(Some(&path)).unwrap_err().to_string();
            assert!(err.contains("absolute"), "{err}");
            let missing = jail.directory().join("missing.toml");
            assert!(matches!(
                Config::load_state_dir(Some(&missing)),
                Err(ConfigError::NotFound(_))
            ));
            Ok(())
        });
    }

    #[test]
    fn default_path_is_under_xdg_config_home() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            std::fs::create_dir_all(jail.directory().join("cfg/hoster"))
                .map_err(|e| e.to_string())?;
            jail.create_file("cfg/hoster/hoster.toml", MINIMAL)?;
            jail.set_env("XDG_CONFIG_HOME", jail.directory().join("cfg").display());
            jail.set_env("XDG_DATA_HOME", jail.directory().join("data").display());
            let config = Config::load(None).map_err(|e| e.to_string())?;
            assert_eq!(config.admin_host.as_ref(), "hoster.example.com");
            Ok(())
        });
    }

    #[test]
    fn missing_file_is_reported_with_its_path() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let path = jail.directory().join("nope.toml");
            let err = Config::load(Some(&path)).unwrap_err();
            assert!(matches!(&err, ConfigError::NotFound(p) if *p == path));
            assert!(err.to_string().contains("nope.toml"));
            Ok(())
        });
    }

    #[test]
    fn unknown_key_is_rejected_and_named() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let problems = invalid(load(
                jail,
                "admin_host = \"hoster.example.com\"\n[proxy]\nthreds = 4\n",
            ));
            assert!(has_problem(&problems, "proxy"), "{problems:?}");
            assert!(problems.iter().any(|p| p.reason.contains("threds")));
            Ok(())
        });
    }

    #[test]
    fn unknown_env_override_is_rejected_and_named() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            jail.set_env("HOSTER_ADMIN_HOTS", "x.example.com");
            let problems = invalid(load(jail, MINIMAL));
            assert!(
                problems.iter().any(|p| p.reason.contains("admin_hots")),
                "{problems:?}"
            );
            Ok(())
        });
    }

    #[test]
    fn env_overrides_take_precedence_over_the_file() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            jail.set_env("HOSTER_ADMIN_HOST", "admin.example.org");
            jail.set_env("HOSTER_ACME__DIRECTORY_URL", "https://staging.example/dir");
            jail.set_env("HOSTER_PROXY__TIMEOUTS__READ", "2m");
            jail.set_env("HOSTER_UPSTREAM_PORTS__FIRST", "30000");
            jail.set_env("HOSTER_UPSTREAM_PORTS__LAST", "30099");
            jail.set_env("HOSTER_STATE_DIR", "/var/lib/hoster");
            let config = load(jail, EXAMPLE).map_err(|e| e.to_string())?;
            assert_eq!(config.admin_host.as_ref(), "admin.example.org");
            assert_eq!(config.acme.directory_url, "https://staging.example/dir");
            assert_eq!(config.proxy.timeouts.read, Duration::from_secs(120));
            // Untouched keys keep the file's values.
            assert_eq!(config.proxy.timeouts.idle, Duration::from_secs(90));
            assert_eq!(config.upstream_ports.first(), 30000);
            assert_eq!(config.upstream_ports.last(), 30099);
            assert_eq!(config.state_dir, Path::new("/var/lib/hoster"));
            assert_eq!(config.webhook_audience(), "https://admin.example.org");
            Ok(())
        });
    }

    #[test]
    fn invalid_durations_name_the_key() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let problems = invalid(load(
                jail,
                "admin_host = \"hoster.example.com\"\n[proxy]\ngrace_period = \"5 parsecs\"\n",
            ));
            assert!(has_problem(&problems, "proxy.grace_period"), "{problems:?}");

            jail.set_env("HOSTER_POLLER__INTERVAL", "soon");
            let problems = invalid(load(jail, MINIMAL));
            assert!(
                problems.iter().any(|p| p.key == "HOSTER_POLLER__INTERVAL"),
                "{problems:?}"
            );

            jail.set_env("HOSTER_POLLER__INTERVAL", "0s");
            let problems = invalid(load(jail, MINIMAL));
            assert!(has_problem(&problems, "poller.interval"), "{problems:?}");
            Ok(())
        });
    }

    #[test]
    fn admin_github_is_required_from_the_environment() {
        Jail::expect_with(|jail| {
            let problems = invalid(load(jail, MINIMAL));
            assert_eq!(
                problems.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
                [ADMIN_GITHUB_ENV]
            );

            jail.set_env(ADMIN_GITHUB_ENV, "-bad-");
            let problems = invalid(load(jail, MINIMAL));
            assert!(has_problem(&problems, ADMIN_GITHUB_ENV), "{problems:?}");
            Ok(())
        });
    }

    #[test]
    fn admin_github_in_the_file_is_rejected() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let problems = invalid(load(
                jail,
                &format!("{MINIMAL}admin_github = \"octocat\"\n"),
            ));
            assert!(has_problem(&problems, "admin_github in"), "{problems:?}");
            Ok(())
        });
    }

    #[test]
    fn port_range_rules() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            for (first, last) in [(80, 2000), (30000, 30014), (30010, 30000)] {
                let toml =
                    format!("{MINIMAL}upstream_ports = {{ first = {first}, last = {last} }}\n");
                let problems = invalid(load(jail, &toml));
                assert!(has_problem(&problems, "upstream_ports"), "{problems:?}");
            }
            let toml = format!("{MINIMAL}upstream_ports = {{ first = 1024, last = 1039 }}\n");
            let config = load(jail, &toml).map_err(|e| e.to_string())?;
            assert_eq!(config.upstream_ports.len(), PortRange::MIN_PORTS);
            assert!(config.upstream_ports.contains(1039));
            assert!(!config.upstream_ports.contains(1040));
            Ok(())
        });
    }

    #[test]
    fn several_problems_are_reported_together() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let toml = "admin_host = \"hoster.example.com\"\n[acme]\ndirectory_url = \"http://insecure\"\ncontact = \"mailto:me\"\nroot_certificate = \"pebble.pem\"\n";
            let problems = invalid(load(jail, toml));
            assert!(has_problem(&problems, "acme.directory_url"), "{problems:?}");
            assert!(has_problem(&problems, "acme.contact"), "{problems:?}");
            assert!(
                has_problem(&problems, "acme.root_certificate"),
                "{problems:?}"
            );
            Ok(())
        });
    }

    #[test]
    fn podman_socket_defaults_to_discovery_and_can_be_set() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(config.podman_socket, None);

            jail.set_env("HOSTER_PODMAN_SOCKET", "/run/user/1000/podman/podman.sock");
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(
                config.podman_socket.as_deref(),
                Some(Path::new("/run/user/1000/podman/podman.sock"))
            );

            jail.set_env("HOSTER_PODMAN_SOCKET", "podman.sock");
            let problems = invalid(load(jail, MINIMAL));
            assert!(has_problem(&problems, "podman_socket"), "{problems:?}");
            Ok(())
        });
    }

    #[test]
    fn webhook_audience_can_be_overridden() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let toml = format!("{MINIMAL}[webhook]\naudience = \"hoster-deploy\"\n");
            let config = load(jail, &toml).map_err(|e| e.to_string())?;
            assert_eq!(config.webhook_audience(), "hoster-deploy");
            Ok(())
        });
    }

    #[test]
    fn the_source_names_the_file_and_the_overridden_keys_without_values() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            jail.set_env("HOSTER_PROXY__TIMEOUTS__READ", "90s");
            jail.set_env("HOSTER_ACME__CONTACT", "ops@example.com");
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(config.source.file, jail.directory().join("hoster.toml"));
            assert_eq!(
                config.source.overrides("admin_github"),
                ["HOSTER_ADMIN_GITHUB"]
            );
            assert_eq!(
                config.source.overrides("proxy.timeouts"),
                ["HOSTER_PROXY__TIMEOUTS__READ"]
            );
            assert_eq!(
                config.source.overrides("proxy"),
                ["HOSTER_PROXY__TIMEOUTS__READ"]
            );
            // A key is not a prefix of another one's name.
            assert!(config.source.overrides("acme.con").is_empty());
            assert!(config.source.overrides("admin_host").is_empty());
            assert_eq!(
                config.source.env_vars(),
                [
                    "HOSTER_ACME__CONTACT",
                    "HOSTER_ADMIN_GITHUB",
                    "HOSTER_PROXY__TIMEOUTS__READ"
                ]
            );
            assert!(
                !format!("{:?}", config.source).contains("ops@example.com"),
                "values are not kept"
            );
            Ok(())
        });
    }

    #[test]
    fn log_format_defaults_to_text_and_can_be_json() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(config.log.format, LogFormat::Text);

            let toml = format!("{MINIMAL}[log]\nformat = \"json\"\n");
            let config = load(jail, &toml).map_err(|e| e.to_string())?;
            assert_eq!(config.log.format, LogFormat::Json);

            jail.set_env("HOSTER_LOG__FORMAT", "text");
            let config = load(jail, &toml).map_err(|e| e.to_string())?;
            assert_eq!(config.log.format, LogFormat::Text);

            jail.set_env("HOSTER_LOG__FORMAT", "yaml");
            let problems = invalid(load(jail, MINIMAL));
            assert!(has_problem(&problems, "HOSTER_LOG__FORMAT"), "{problems:?}");
            Ok(())
        });
    }

    #[test]
    fn web_dev_listen_must_be_loopback() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(config.web.dev_listen, None);

            jail.set_env("HOSTER_WEB__DEV_LISTEN", "127.0.0.1:8080");
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(
                config.web.dev_listen,
                Some("127.0.0.1:8080".parse().unwrap())
            );

            jail.set_env("HOSTER_WEB__DEV_LISTEN", "0.0.0.0:8080");
            let problems = invalid(load(jail, MINIMAL));
            assert!(has_problem(&problems, "web.dev_listen"), "{problems:?}");
            Ok(())
        });
    }

    #[test]
    fn web_oauth_files_and_idle_timeout() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(config.web.session_idle_timeout, Duration::from_secs(3600));
            assert_eq!(config.admin_origin(), "https://hoster.example.com");

            jail.set_env("HOSTER_WEB__DEV_LISTEN", "127.0.0.1:8080");
            jail.set_env("HOSTER_WEB__SESSION_IDLE_TIMEOUT", "30m");
            jail.set_env("HOSTER_WEB__GITHUB_CLIENT_ID_FILE", "/etc/hoster/client-id");
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(config.web.session_idle_timeout, Duration::from_secs(1800));
            assert_eq!(config.admin_origin(), "http://127.0.0.1:8080");
            assert_eq!(
                config.web.github_client_id_file.as_deref(),
                Some(Path::new("/etc/hoster/client-id"))
            );

            jail.set_env("HOSTER_WEB__SESSION_IDLE_TIMEOUT", "13h");
            jail.set_env("HOSTER_WEB__GITHUB_CLIENT_SECRET_FILE", "secret");
            let problems = invalid(load(jail, MINIMAL));
            assert!(
                has_problem(&problems, "web.session_idle_timeout"),
                "{problems:?}"
            );
            assert!(
                has_problem(&problems, "web.github_client_secret_file"),
                "{problems:?}"
            );
            Ok(())
        });
    }

    #[test]
    fn secrets_are_read_from_a_private_file_before_the_credential() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret");
        std::fs::write(&file, "from-file\n").unwrap();
        let creds = Credentials::default();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = creds.read_or_file("x", Some(&file)).unwrap_err();
        assert!(format!("{err:#}").contains("secret"), "{err:#}");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            creds.read_or_file("x", Some(&file)).unwrap().as_deref(),
            Some("from-file")
        );
        assert!(
            creds
                .read_or_file("x", Some(&dir.path().join("missing")))
                .is_err()
        );
        assert_eq!(creds.read_or_file("x", None).unwrap(), None);
    }

    #[test]
    fn hostname_validation() {
        for ok in ["hoster.example.com", "a-b.c0.example", "x.io"] {
            assert!(ok.parse::<Hostname>().is_ok(), "{ok}");
        }
        for bad in [
            "",
            "localhost",
            "Hoster.example.com",
            "hoster.example.com.",
            "-a.example.com",
            "a..example.com",
            "a_b.example.com",
            "10.0.0.1",
            "https://hoster.example.com",
        ] {
            assert!(bad.parse::<Hostname>().is_err(), "{bad}");
        }
    }

    #[test]
    fn admin_host_must_be_a_lowercase_fqdn() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let problems = invalid(load(jail, "admin_host = \"Hoster.Example.com\"\n"));
            assert!(has_problem(&problems, "admin_host"), "{problems:?}");
            assert!(problems.iter().any(|p| p.reason.contains("lowercase")));
            Ok(())
        });
    }

    #[test]
    fn github_handle_matches_case_insensitively() {
        let handle: GithubHandle = "OctoCat".parse().unwrap();
        assert!(handle.matches("octocat"));
        assert!(!handle.matches("octocat2"));
        assert!("a--b".parse::<GithubHandle>().is_err());
        assert!("x".repeat(40).parse::<GithubHandle>().is_err());
    }

    #[test]
    fn credentials_are_read_from_the_credentials_directory() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            std::fs::create_dir_all(jail.directory().join("creds")).map_err(|e| e.to_string())?;
            jail.create_file("creds/oauth_client_secret", "s3cret\n")?;
            jail.set_env(
                "CREDENTIALS_DIRECTORY",
                jail.directory().join("creds").display(),
            );
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            let read = |name| config.credentials.read(name).map_err(|e| e.to_string());
            assert_eq!(read("oauth_client_secret")?.as_deref(), Some("s3cret"));
            assert_eq!(read("missing")?, None);
            assert!(read("../hoster.toml").is_err());
            assert_eq!(Credentials::default().read("anything").ok(), Some(None));
            Ok(())
        });
    }

    #[test]
    fn errors_render_one_problem_per_line() {
        let err = ConfigError::Invalid(Problems(vec![
            Problem::new("a.b", "bad"),
            Problem::new("HOSTER_C", "worse"),
        ]));
        assert_eq!(
            err.to_string(),
            "invalid configuration:\n  a.b: bad\n  HOSTER_C: worse"
        );
    }

    #[test]
    fn string_overrides_keep_numeric_looking_text() {
        Jail::expect_with(|jail| {
            jail.set_env(ADMIN_GITHUB_ENV, "007");
            jail.set_env("HOSTER_WEBHOOK__AUDIENCE", "12345");
            jail.set_env("HOSTER_ACME__CONTACT", "1@2.example");
            jail.set_env("HOSTER_PROXY__THREADS", "3");
            jail.set_env("HOSTER_PROXY__HTTP_LISTEN", "127.0.0.1:8080");
            jail.set_env("HOSTER_PROXY__HTTPS_LISTEN", "127.0.0.1:8443");
            jail.set_env("HOSTER_ACME__PUBLIC_ADDRESSES", "[\"192.0.2.1\", \"::1\"]");
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(config.admin_github.as_ref(), "007");
            assert_eq!(config.webhook_audience(), "12345");
            assert_eq!(config.acme.contact.as_deref(), Some("1@2.example"));
            assert_eq!(config.proxy.threads.get(), 3);
            assert_eq!(config.proxy.http_listen.to_string(), "127.0.0.1:8080");
            assert_eq!(config.proxy.https_listen.to_string(), "127.0.0.1:8443");
            assert_eq!(config.acme.public_addresses.len(), 2);
            Ok(())
        });
    }

    #[test]
    fn one_end_of_the_port_range_can_be_overridden_alone() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            jail.set_env("HOSTER_UPSTREAM_PORTS__LAST", "24999");
            let config = load(jail, MINIMAL).map_err(|e| e.to_string())?;
            assert_eq!(config.upstream_ports.to_string(), "20000-24999");
            Ok(())
        });
    }

    #[test]
    fn unreadable_file_reports_only_the_file() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            let problems = invalid(load(jail, "admin_host = \"hoster.example.com\n"));
            assert_eq!(problems.len(), 1, "{problems:?}");
            assert!(problems[0].key.ends_with("hoster.toml"), "{problems:?}");
            Ok(())
        });
    }

    #[test]
    fn pingora_shutdown_durations_are_whole_seconds() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            jail.set_env("HOSTER_PROXY__GRACE_PERIOD", "1500ms");
            let problems = invalid(load(jail, MINIMAL));
            assert!(has_problem(&problems, "proxy.grace_period"), "{problems:?}");
            Ok(())
        });
    }

    #[test]
    fn http_listen_needs_a_port() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            jail.set_env("HOSTER_PROXY__HTTP_LISTEN", "127.0.0.1:0");
            let problems = invalid(load(jail, MINIMAL));
            assert!(has_problem(&problems, "proxy.http_listen"), "{problems:?}");
            Ok(())
        });
    }

    #[test]
    fn https_listen_needs_its_own_port() {
        Jail::expect_with(|jail| {
            with_admin(jail);
            jail.set_env("HOSTER_PROXY__HTTPS_LISTEN", "127.0.0.1:0");
            let problems = invalid(load(jail, MINIMAL));
            assert!(has_problem(&problems, "proxy.https_listen"), "{problems:?}");

            jail.set_env("HOSTER_PROXY__HTTP_LISTEN", "127.0.0.1:8080");
            jail.set_env("HOSTER_PROXY__HTTPS_LISTEN", "127.0.0.1:8080");
            let problems = invalid(load(jail, MINIMAL));
            assert!(has_problem(&problems, "proxy.https_listen"), "{problems:?}");
            Ok(())
        });
    }
}
