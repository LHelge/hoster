//! Pass 1 (strict serde into span-carrying types) and pass 2 (semantic validation).
//!
//! Every raw type has a `validate` method that reports problems to a [`Validator`] and returns
//! the model value when its own part was valid, so one run reports all errors.
//!
//! The raw types also derive the stack file's JSON Schema (`schema.rs`): their field docs are
//! what editors show on hover, so they are written for stack file authors. `Spanned` fields name
//! their schema with `schema_with`, since the span wrapper is invisible in the file.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::str::FromStr;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use toml::Spanned;

use super::dependency::Graph;
use super::schema::Field;
use super::{
    ConfigFile, ContainerPath, DEFAULT_DRAIN_TIMEOUT, DEFAULT_HEALTH_PATH, DEFAULT_STOP_TIMEOUT,
    DeployAuth, Diagnostic, ExpectStatus, FieldPath, FileMode, FileOwner, FileSource, HealthCheck,
    Host, ImageRef, Location, Name, NormalizedPrefix, ParsedStack, Probe, Route, RouteTimeouts,
    SUPPORTED_VERSION, Service, Stack, StackError, UpdateStrategy,
};
use crate::health;

type Str = Spanned<String>;
type Int = Spanned<i64>;

/// A hoster stack file (`<stack>.stack.toml`): one or more services deployed together on one
/// podman network. See https://docs.hoster.lhelge.se/wiki/stack-format/.
///
/// The schema checks each field on its own. hoster checks the rules that need more than one value
/// when the stack is applied: a (host, prefix) used twice within the stack or by another stack;
/// `depends_on` cycles, unknown services or a service naming itself; secrets of another stack; a
/// name set in both `env` and `secrets`; a mount path used twice; a config file in, above or at a
/// volume, above another file or at a path podman manages, and the 256 KiB per file and 1 MiB per
/// stack limits on inline `content`; a tag or digest on images other
/// than CI-built `ghcr.io` ones, and `track` only on those; `http` and `tcp` checks only on
/// routed services, against a route's port; a timings-only `health` table only on routed
/// services. It also rejects durations that overflow or whose fraction is not a whole number of
/// nanoseconds, such as `"0.001h"`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "hoster stack file")]
pub(super) struct RawStack {
    /// Schema version of the stack file. Must be `1`.
    #[schemars(schema_with = "Field::version")]
    version: Int,
    /// Stack name, unique per hoster instance: a lowercase letter, then lowercase letters, digits
    /// and `-`, at most 31 characters. Used in podman object names (`hoster-<name>`).
    #[schemars(schema_with = "Field::name")]
    name: Str,
    /// Upper bound for draining the old containers' in-flight requests after a deploy (ADR-0017),
    /// as a duration such as `"30s"`.
    #[serde(default)]
    #[schemars(
        schema_with = "Field::duration",
        extend("default" = Field::default_duration(DEFAULT_DRAIN_TIMEOUT))
    )]
    drain_timeout: Option<Str>,
    /// Who may deploy this stack through the webhook (ADR-0047). Without it, the stack can only
    /// be deployed from the admin UI.
    #[serde(default)]
    #[schemars(with = "RawDeploy")]
    deploy: Option<RawDeploy>,
    /// The stack's services, at least one. Keys are service names (a lowercase letter, then
    /// lowercase letters, digits and `-`, at most 31 characters) and DNS aliases on the stack
    /// network.
    #[schemars(schema_with = "Field::services::<RawService>")]
    services: Spanned<BTreeMap<String, Spanned<RawService>>>,
}

/// Deploy authorization: which GitHub Actions OIDC tokens may deploy this stack through the
/// webhook (ADR-0047).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Deploy")]
struct RawDeploy {
    /// GitHub's numeric repository id (stable across renames), compared with the token's
    /// `repository_id` claim. `gh api repos/<owner>/<repo> --jq .id` prints it.
    #[schemars(schema_with = "Field::repository_id")]
    repository_id: Int,
    /// Glob patterns for the token's `ref`, at least one of which must match, e.g.
    /// `["refs/tags/v*"]`. `*` matches within one path segment, `**` across segments. Omit to
    /// allow any ref.
    #[serde(default)]
    #[schemars(schema_with = "Field::refs")]
    refs: Option<Spanned<Vec<Str>>>,
    /// Must equal the token's `environment` claim. Pair it with GitHub environment protection
    /// rules.
    #[serde(default)]
    #[schemars(schema_with = "Field::non_empty")]
    environment: Option<Str>,
    /// Must equal the token's `job_workflow_ref` claim, e.g.
    /// `"acme/notes/.github/workflows/release.yml@refs/heads/main"`.
    #[serde(default)]
    #[schemars(schema_with = "Field::non_empty")]
    job_workflow_ref: Option<Str>,
}

/// One container of the stack.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Service")]
struct RawService {
    /// Image reference with an explicit registry. CI-built `ghcr.io` images omit the tag, e.g.
    /// `"ghcr.io/acme/notes-api"`: their digest comes from the deploy. Third-party images need
    /// `repo:tag`, `repo:tag@sha256:…` or `repo@sha256:…`; a tag is resolved to a digest when the
    /// stack is applied (ADR-0025).
    #[schemars(schema_with = "Field::image")]
    image: Str,
    /// Tag the ghcr poller watches, e.g. `"main"` (ADR-0043). Only for CI-built `ghcr.io` images
    /// without a tag or digest.
    #[serde(default)]
    #[schemars(schema_with = "Field::tag")]
    track: Option<Str>,
    /// Overrides the image's `CMD`.
    #[serde(default)]
    #[schemars(schema_with = "Field::strings")]
    command: Option<Vec<String>>,
    /// Overrides the image's `ENTRYPOINT`.
    #[serde(default)]
    #[schemars(schema_with = "Field::strings")]
    entrypoint: Option<Vec<String>>,
    /// Plain, non-secret environment variables. A routed service with exactly one container port
    /// also gets `PORT=<that port>` unless it sets `PORT` itself (ADR-0051).
    #[serde(default)]
    #[schemars(schema_with = "Field::env")]
    env: BTreeMap<String, Str>,
    /// Environment variable name → secret name `<stack>/<name>` in hoster's encrypted secret
    /// store (ADR-0029), e.g. `DATABASE_URL = "notes/database-url"`. The `<stack>` part must be
    /// this stack's name, and a name must not be set in both `env` and `secrets`.
    #[serde(default)]
    #[schemars(schema_with = "Field::secrets")]
    secrets: BTreeMap<String, Str>,
    /// Named podman volume → absolute mount path, e.g. `data = "/data"` (ADR-0030). Volumes are
    /// kept when the stack is removed. Each path may be used once.
    #[serde(default)]
    #[schemars(schema_with = "Field::volumes")]
    volumes: BTreeMap<String, Str>,
    /// Config files: absolute container path → its content, e.g.
    /// `"/etc/app/config.yml" = { content = "…" }` (ADR-0057). hoster writes them into each
    /// container before it starts; they need podman 5.4 or later. A path may not be in or above a
    /// volume, above another file, in `/proc`, `/sys` or `/dev`, or a file podman writes itself
    /// (`/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`, `/etc/passwd`, `/etc/group`,
    /// `/run/.containerenv`).
    #[serde(default)]
    #[schemars(schema_with = "Field::files::<RawFile>")]
    files: BTreeMap<String, Spanned<RawFile>>,
    /// Health check (ADR-0038, ADR-0056). Without it, a routed service is probed with
    /// `GET /healthz` on its first route's port, and any other service counts as healthy once
    /// running for 5 s.
    #[serde(default)]
    #[schemars(with = "RawHealth")]
    health: Option<Spanned<RawHealth>>,
    /// How the service is replaced on deploy (ADR-0039).
    #[serde(default)]
    #[schemars(extend("default" = "start-first"))]
    update: UpdateStrategy,
    /// Services of this stack that must pass their health check before this one starts, e.g.
    /// `["db", "cache"]` (ADR-0056). Each names another service of the stack, at most once,
    /// without cycles.
    #[serde(default)]
    #[schemars(schema_with = "Field::depends_on")]
    depends_on: Vec<Str>,
    /// SIGTERM → SIGKILL grace period when the container is stopped, as a duration such as
    /// `"10s"`.
    #[serde(default)]
    #[schemars(
        schema_with = "Field::duration",
        extend("default" = Field::default_duration(DEFAULT_STOP_TIMEOUT))
    )]
    stop_timeout: Option<Str>,
    /// `uid[:gid]` inside the container, each numeric or a name.
    #[serde(default)]
    #[schemars(schema_with = "Field::user")]
    user: Option<Str>,
    /// Read-only root filesystem.
    #[serde(default)]
    #[schemars(extend("default" = false))]
    read_only: bool,
    /// Routes from a (host, prefix) to a container port (ADR-0007). Without routes the service
    /// publishes no ports.
    #[serde(default)]
    routes: Vec<RawRoute>,
}

/// A config file (ADR-0057): exactly one of `content` and `secret`, plus who may read it. The
/// container's user cannot change it unless that user is root; with `read_only = true` nobody
/// can.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "File")]
#[schemars(extend("oneOf" = [{ "required": ["content"] }, { "required": ["secret"] }]))]
struct RawFile {
    /// The file's text, at most 256 KiB, and 1 MiB for all files of the stack together. A TOML
    /// multi-line string (`"""…"""`, or `'''…'''` to keep backslashes) holds a whole file.
    #[serde(default)]
    #[schemars(with = "String")]
    content: Option<Str>,
    /// A secret `<stack>/<name>` of this stack whose value becomes the file's content
    /// (ADR-0029). Changing the secret redeploys the service.
    #[serde(default)]
    #[schemars(schema_with = "Field::secret")]
    secret: Option<Str>,
    /// Permission bits as four octal digits, `"0000"` to `"0777"`. Default `"0444"` for
    /// `content` and `"0400"` for `secret`.
    #[serde(default)]
    #[schemars(schema_with = "Field::file_mode")]
    mode: Option<Str>,
    /// Numeric `"uid:gid"` inside the container, each at most 65535. Default `"0:0"` for
    /// `content`, and the user the container runs as for `secret`. Set it for images that start
    /// as root and switch user themselves (nginx, postgres): podman sees those as root.
    #[serde(default)]
    #[schemars(schema_with = "Field::file_owner")]
    owner: Option<Str>,
}

/// A (host, prefix) → container port route (ADR-0007, ADR-0009, ADR-0010). A (host, prefix)
/// must be unique within the stack and across stacks (ADR-0012).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Route")]
struct RawRoute {
    /// A lowercase fully qualified domain name without port, scheme or trailing dot, e.g.
    /// `"notes.example.com"`. No wildcards or IP addresses. It must resolve to this server for
    /// its certificate.
    #[schemars(schema_with = "Field::host")]
    host: Str,
    /// Path prefix the route matches on segment boundaries (`/api` matches `/api/x`, not
    /// `/apix`). Starts with `/`, no trailing `/` except the root, no `?`, `#`, `%`, `//`, `.` or
    /// `..` segments.
    #[serde(default)]
    #[schemars(schema_with = "Field::prefix", extend("default" = "/"))]
    prefix: Option<Str>,
    /// Remove the matched prefix before forwarding (ADR-0010). No effect on `/`.
    #[serde(default)]
    #[schemars(with = "bool", extend("default" = false))]
    strip_prefix: Option<Spanned<bool>>,
    /// Container port the app listens on, 1–65535. hoster publishes it on an allocated
    /// `127.0.0.1` port; the app must bind `0.0.0.0` or `::`.
    #[schemars(schema_with = "Field::port")]
    port: Int,
    /// Proxy timeouts for this route (ADR-0011); unset ones use hoster's defaults.
    #[serde(default)]
    timeouts: RawTimeouts,
}

/// Per-route proxy timeouts (ADR-0011), as durations greater than zero such as `"60s"`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Timeouts")]
struct RawTimeouts {
    /// Connecting to the container.
    #[serde(default)]
    #[schemars(schema_with = "Field::positive_duration")]
    connect: Option<Str>,
    /// Between reads of the response.
    #[serde(default)]
    #[schemars(schema_with = "Field::positive_duration")]
    read: Option<Str>,
    /// Between writes of the request.
    #[serde(default)]
    #[schemars(schema_with = "Field::positive_duration")]
    write: Option<Str>,
    /// Idle keep-alive connections.
    #[serde(default)]
    #[schemars(schema_with = "Field::positive_duration")]
    idle: Option<Str>,
}

/// A health check (ADR-0038, ADR-0056): at most one of `http`, `tcp` and `exec`, plus timings.
/// `http` and `tcp` need a route; a service without routes may only declare `exec`. A
/// timings-only table on a routed service tunes the default `GET /healthz` probe.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "Health")]
#[schemars(extend("dependentSchemas" = {
    "http": { "properties": { "tcp": false, "exec": false } },
    "tcp": { "properties": { "http": false, "exec": false } },
}))]
struct RawHealth {
    /// An HTTP `GET` from the host against the route's loopback port. Redirects are not
    /// followed.
    #[serde(default)]
    #[schemars(with = "RawHttpProbe")]
    http: Option<Spanned<RawHttpProbe>>,
    /// A TCP connect from the host against the route's loopback port.
    #[serde(default)]
    #[schemars(with = "RawTcpProbe")]
    tcp: Option<Spanned<RawTcpProbe>>,
    /// A command run inside the container without a shell, e.g. `["pg_isready", "-U", "app"]`;
    /// exit 0 passes. Use `["sh", "-c", "…"]` when it needs a shell.
    #[serde(default)]
    #[schemars(schema_with = "Field::exec")]
    exec: Option<Spanned<Vec<Str>>>,
    /// Time between probes, greater than zero.
    #[serde(default)]
    #[schemars(
        schema_with = "Field::positive_duration",
        extend("default" = Field::default_duration(health::DEFAULT_INTERVAL))
    )]
    interval: Option<Str>,
    /// How long one probe may take, greater than zero. Default `"2s"`, `"5s"` for `exec`.
    #[serde(default)]
    #[schemars(schema_with = "Field::positive_duration")]
    timeout: Option<Str>,
    /// Consecutive successful probes needed to count as healthy.
    #[serde(default)]
    #[schemars(
        schema_with = "Field::positive_integer",
        extend("default" = health::DEFAULT_HEALTHY_AFTER)
    )]
    healthy_after: Option<Int>,
    /// How long a deploy waits for the service to become healthy before it aborts, greater than
    /// zero.
    #[serde(default)]
    #[schemars(
        schema_with = "Field::positive_duration",
        extend("default" = Field::default_duration(health::DEFAULT_START_PERIOD))
    )]
    start_period: Option<Str>,
}

/// An HTTP health probe.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "HttpProbe")]
struct RawHttpProbe {
    /// Request path, sent as written: starts with `/`, with spaces, control and non-ASCII
    /// characters percent-encoded.
    #[serde(default)]
    #[schemars(
        schema_with = "Field::health_path",
        extend("default" = DEFAULT_HEALTH_PATH)
    )]
    path: Option<Str>,
    /// Container port to probe; must be the port of one of the service's routes. Defaults to
    /// the first route's port.
    #[serde(default)]
    #[schemars(schema_with = "Field::port")]
    port: Option<Int>,
    /// Healthy status codes: a class such as `"2xx"` or one code such as `"204"`. Default
    /// 2xx–3xx.
    #[serde(default)]
    #[schemars(schema_with = "Field::expect_status")]
    expect_status: Option<Str>,
}

/// A TCP health probe.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "TcpProbe")]
struct RawTcpProbe {
    /// Container port to probe; must be the port of one of the service's routes. Defaults to
    /// the first route's port.
    #[serde(default)]
    #[schemars(schema_with = "Field::port")]
    port: Option<Int>,
}

/// Collects diagnostics during validation.
struct Validator<'s> {
    source: &'s str,
    errors: Vec<Diagnostic>,
    warnings: Vec<Diagnostic>,
}

impl<'s> Validator<'s> {
    fn diagnostic(&self, path: &FieldPath, span: Range<usize>, message: String) -> Diagnostic {
        Diagnostic {
            path: path.clone(),
            location: Some(Location::of(self.source, span.start)),
            message,
        }
    }

    fn error(&mut self, path: &FieldPath, span: Range<usize>, message: impl Into<String>) {
        let diagnostic = self.diagnostic(path, span, message.into());
        self.errors.push(diagnostic);
    }

    fn warn(&mut self, path: &FieldPath, span: Range<usize>, message: impl Into<String>) {
        let diagnostic = self.diagnostic(path, span, message.into());
        self.warnings.push(diagnostic);
    }

    /// Parses a spanned string with `FromStr`, reporting the error at its span.
    fn parse<T: FromStr<Err = String>>(&mut self, path: &FieldPath, value: &Str) -> Option<T> {
        self.parse_at(path, value.get_ref(), value.span())
    }

    /// Parses `text` with `FromStr`, reporting the error at `span`.
    fn parse_at<T: FromStr<Err = String>>(
        &mut self,
        path: &FieldPath,
        text: &str,
        span: Range<usize>,
    ) -> Option<T> {
        text.parse().map_err(|e| self.error(path, span, e)).ok()
    }

    fn duration(&mut self, path: &FieldPath, value: &Str) -> Option<Duration> {
        humantime::parse_duration(value.get_ref())
            .map_err(|e| {
                self.error(
                    path,
                    value.span(),
                    format!(
                        "invalid duration `{}`: {e}; expected e.g. \"500ms\", \"30s\" or \"5m\"",
                        value.get_ref()
                    ),
                )
            })
            .ok()
    }

    fn optional_duration(
        &mut self,
        parent: &FieldPath,
        key: &str,
        value: &Option<Str>,
    ) -> Option<Option<Duration>> {
        match value {
            Some(value) => self.duration(&parent.key(key), value).map(Some),
            None => Some(None),
        }
    }

    fn positive_duration(
        &mut self,
        parent: &FieldPath,
        key: &str,
        value: &Option<Str>,
    ) -> Option<Option<Duration>> {
        let duration = self.optional_duration(parent, key, value)?;
        if let (Some(value), Some(Duration::ZERO)) = (value, duration) {
            self.error(&parent.key(key), value.span(), "must be greater than zero");
            return None;
        }
        Some(duration)
    }

    fn port(&mut self, path: &FieldPath, value: &Int) -> Option<u16> {
        match u16::try_from(*value.get_ref()) {
            Ok(port) if port > 0 => Some(port),
            _ => {
                self.error(
                    path,
                    value.span(),
                    format!("invalid port {}: must be 1-65535", value.get_ref()),
                );
                None
            }
        }
    }

    fn non_empty(&mut self, path: &FieldPath, value: &Str) -> Option<String> {
        if value.get_ref().trim().is_empty() {
            self.error(path, value.span(), "must not be empty");
            None
        } else {
            Some(value.get_ref().clone())
        }
    }
}

impl RawStack {
    pub(super) fn parse(source: &str) -> Result<ParsedStack, StackError> {
        let raw = Self::deserialize_from(source)?;
        let mut validator = Validator {
            source,
            errors: Vec::new(),
            warnings: Vec::new(),
        };
        match raw.validate(&mut validator) {
            Some(stack) if validator.errors.is_empty() => {
                validator.warnings.sort_by_key(|d| d.location);
                Ok(ParsedStack {
                    stack,
                    warnings: validator.warnings,
                })
            }
            _ => Err(StackError::new(validator.errors)),
        }
    }

    fn deserialize_from(source: &str) -> Result<Self, StackError> {
        let locate = |e: &toml::de::Error| e.span().map(|span| Location::of(source, span.start));
        let deserializer = toml::Deserializer::parse(source).map_err(|e| {
            StackError::new(vec![Diagnostic {
                path: FieldPath::default(),
                location: locate(&e),
                message: e.message().to_owned(),
            }])
        })?;
        serde_path_to_error::deserialize(deserializer).map_err(|e| {
            StackError::new(vec![Diagnostic {
                path: FieldPath::from(e.path()),
                location: locate(e.inner()),
                message: e.inner().message().to_owned(),
            }])
        })
    }

    fn validate(self, v: &mut Validator<'_>) -> Option<Stack> {
        let root = FieldPath::default();
        if *self.version.get_ref() != SUPPORTED_VERSION {
            v.error(
                &root.key("version"),
                self.version.span(),
                format!(
                    "unsupported version {}; this hoster supports version {SUPPORTED_VERSION}",
                    self.version.get_ref()
                ),
            );
        }
        let name: Option<Name> = v.parse(&root.key("name"), &self.name);
        let drain_timeout = v
            .optional_duration(&root, "drain_timeout", &self.drain_timeout)
            .map(|d| d.unwrap_or(DEFAULT_DRAIN_TIMEOUT));
        let deploy = match self.deploy {
            Some(deploy) => deploy.validate(v, &root.key("deploy")).map(Some),
            None => Some(None),
        };

        let services_path = root.key("services");
        if self.services.get_ref().is_empty() {
            v.error(
                &services_path,
                self.services.span(),
                "a stack needs at least one service",
            );
        }
        Self::check_dependency_cycles(v, &services_path, self.services.get_ref());
        Self::check_file_total(v, &services_path, self.services.get_ref());
        let keys: BTreeSet<String> = self.services.get_ref().keys().cloned().collect();
        let mut services = BTreeMap::new();
        let mut all_valid = true;
        for (key, raw) in self.services.into_inner() {
            let path = services_path.key(&key);
            let service_name: Option<Name> = v.parse_at(&path, &key, raw.span());
            let context = ServiceContext {
                stack: name.as_ref(),
                key: &key,
                services: &keys,
            };
            let service = raw.into_inner().validate(v, &path, context);
            match (service_name, service) {
                (Some(service_name), Some(service)) => {
                    services.insert(service_name, service);
                }
                _ => all_valid = false,
            }
        }

        Some(Stack {
            name: name?,
            drain_timeout: drain_timeout?,
            deploy: deploy?,
            services: all_valid.then_some(services)?,
        })
    }

    /// Inline file content is capped per stack (ADR-0057 §2): reported once, at the file that
    /// takes the total over the limit. Files over the per-file limit are their own error and
    /// count as nothing here.
    fn check_file_total(
        v: &mut Validator<'_>,
        services_path: &FieldPath,
        services: &BTreeMap<String, Spanned<RawService>>,
    ) {
        let mut total = 0;
        for (key, service) in services {
            for (file, raw) in &service.get_ref().files {
                let Some(content) = &raw.get_ref().content else {
                    continue;
                };
                let len = content.get_ref().len();
                if len > ConfigFile::MAX_BYTES {
                    continue;
                }
                total += len;
                if total > ConfigFile::MAX_STACK_BYTES {
                    v.error(
                        &services_path.key(key).key("files").key(file).key("content"),
                        content.span(),
                        format!(
                            "the stack's inline file content is over {} KiB in total",
                            ConfigFile::MAX_STACK_BYTES / 1024
                        ),
                    );
                    return;
                }
            }
        }
    }

    /// Reports each dependency cycle once, at the `depends_on` entry of its smallest member
    /// that starts it. Unknown names and self references are the services' own errors.
    fn check_dependency_cycles(
        v: &mut Validator<'_>,
        services_path: &FieldPath,
        services: &BTreeMap<String, Spanned<RawService>>,
    ) {
        let graph = Graph::new(services.iter().map(|(key, raw)| {
            let deps = raw
                .get_ref()
                .depends_on
                .iter()
                .map(|d| d.get_ref().as_str());
            (key.as_str(), deps.filter(move |d| d != key))
        }));
        for cycle in graph.cycles() {
            let (start, next) = (cycle[0], cycle[1 % cycle.len()]);
            let entries = &services[start].get_ref().depends_on;
            let Some((i, entry)) = entries
                .iter()
                .enumerate()
                .find(|(_, d)| d.get_ref() == next)
            else {
                continue;
            };
            let chain: Vec<&str> = cycle.iter().chain([&start]).copied().collect();
            v.error(
                &services_path.key(start).key("depends_on").index(i),
                entry.span(),
                format!("dependency cycle: {}", chain.join(" → ")),
            );
        }
    }
}

/// What a service's validation needs to know about the stack around it.
#[derive(Clone, Copy)]
struct ServiceContext<'a> {
    /// The stack's name, when it is valid.
    stack: Option<&'a Name>,
    /// This service's key.
    key: &'a str,
    /// Every service key of the stack, valid or not.
    services: &'a BTreeSet<String>,
}

impl RawDeploy {
    fn validate(self, v: &mut Validator<'_>, path: &FieldPath) -> Option<DeployAuth> {
        let repository_id = match u64::try_from(*self.repository_id.get_ref()) {
            Ok(id) if id > 0 => Some(id),
            _ => {
                v.error(
                    &path.key("repository_id"),
                    self.repository_id.span(),
                    "must be a positive integer (GitHub's numeric repository id)",
                );
                None
            }
        };
        let mut valid = true;
        // An explicit empty list would read as "no ref may deploy" under ADR-0047's "at least
        // one must match", but the model stores "no restriction" as an empty list. Reject it so
        // the two can't be confused.
        if let Some(raw) = self.refs.as_ref().filter(|raw| raw.get_ref().is_empty()) {
            v.error(
                &path.key("refs"),
                raw.span(),
                "must list at least one pattern; omit `refs` to allow any ref",
            );
            valid = false;
        }
        let raw_refs = self.refs.map(Spanned::into_inner).unwrap_or_default();
        let mut refs = Vec::with_capacity(raw_refs.len());
        for (i, pattern) in raw_refs.iter().enumerate() {
            if pattern.get_ref().starts_with("refs/") {
                refs.push(pattern.get_ref().clone());
            } else {
                v.error(
                    &path.key("refs").index(i),
                    pattern.span(),
                    format!(
                        "ref pattern `{}` must start with `refs/`, e.g. \"refs/tags/v*\"",
                        pattern.get_ref()
                    ),
                );
                valid = false;
            }
        }
        let environment = match &self.environment {
            Some(env) => v.non_empty(&path.key("environment"), env).map(Some),
            None => Some(None),
        };
        let job_workflow_ref = match &self.job_workflow_ref {
            Some(r) => v.non_empty(&path.key("job_workflow_ref"), r).map(Some),
            None => Some(None),
        };
        valid.then_some(())?;
        Some(DeployAuth {
            repository_id: repository_id?,
            refs,
            environment: environment?,
            job_workflow_ref: job_workflow_ref?,
        })
    }
}

impl RawService {
    fn validate(
        self,
        v: &mut Validator<'_>,
        path: &FieldPath,
        context: ServiceContext<'_>,
    ) -> Option<Service> {
        let stack = context.stack;
        let mut valid = true;

        let image: Option<ImageRef> = v.parse(&path.key("image"), &self.image);
        if let Some(image) = &image
            && image.tag().is_none()
            && image.digest().is_none()
            && !image.is_ci_built()
        {
            v.error(
                &path.key("image"),
                self.image.span(),
                format!(
                    "third-party image `{image}` needs a tag or digest (`repo:tag`, \
                         `repo:tag@sha256:…` or `repo@sha256:…`); only CI-built ghcr.io \
                         images may omit both"
                ),
            );
            valid = false;
        }

        let track = match &self.track {
            None => None,
            Some(track) => {
                let track_path = path.key("track");
                if image.as_ref().is_some_and(|image| !image.is_ci_built()) {
                    v.error(
                        &track_path,
                        track.span(),
                        "`track` is only allowed on CI-built ghcr.io images without a tag or \
                         digest",
                    );
                    valid = false;
                } else if !ImageRef::is_valid_tag(track.get_ref()) {
                    v.error(
                        &track_path,
                        track.span(),
                        format!("invalid tag `{}`", track.get_ref()),
                    );
                    valid = false;
                }
                Some(track.get_ref().clone())
            }
        };

        let mut env = BTreeMap::new();
        for (key, value) in self.env {
            if !Self::is_env_name(&key) {
                v.error(
                    &path.key("env").key(&key),
                    value.span(),
                    format!("invalid environment variable name `{}`", key),
                );
                valid = false;
            }
            env.insert(key, value.into_inner());
        }

        let mut secrets = BTreeMap::new();
        for (key, value) in self.secrets {
            let secret_path = path.key("secrets").key(&key);
            if !Self::is_env_name(&key) {
                v.error(
                    &secret_path,
                    value.span(),
                    format!("invalid environment variable name `{}`", key),
                );
                valid = false;
            } else if env.contains_key(&key) {
                v.error(
                    &secret_path,
                    value.span(),
                    format!("`{}` is set in both `env` and `secrets`", key),
                );
                valid = false;
            }
            if !Self::is_secret_ref(value.get_ref(), stack) {
                v.error(
                    &secret_path,
                    value.span(),
                    Self::secret_ref_error(value.get_ref(), stack),
                );
                valid = false;
            }
            secrets.insert(key, value.into_inner());
        }

        if self.update == UpdateStrategy::StartFirst
            && let Some(first) = self.volumes.values().next()
        {
            // ADR-0030, ADR-0039: during a start-first deploy two instances mount the volume.
            v.warn(
                &path.key("volumes"),
                first.span(),
                "this service has volumes but uses `update = \"start-first\"`: during a deploy the \
                 old and the new container mount the same volume at once; set \
                 `update = \"stop-first\"` unless the app is safe with that",
            );
        }
        let mut volumes = BTreeMap::new();
        let mut mount_paths = BTreeSet::new();
        for (key, target) in self.volumes {
            let volume_path = path.key("volumes").key(&key);
            if v.parse_at::<Name>(&volume_path, &key, target.span())
                .is_none()
            {
                valid = false;
            }
            let target_str = target.get_ref();
            if !Self::is_mount_path(target_str) {
                v.error(
                    &volume_path,
                    target.span(),
                    format!(
                        "invalid mount path `{target_str}`: must be an absolute path below `/` \
                         without empty, `.` or `..` segments, a trailing `/`, `:` or `,`"
                    ),
                );
                valid = false;
            } else if !mount_paths.insert(target_str.clone()) {
                v.error(
                    &volume_path,
                    target.span(),
                    format!("mount path `{target_str}` is used by more than one volume"),
                );
                valid = false;
            }
            volumes.insert(key, target.into_inner());
        }

        let files_path = path.key("files");
        let mut files = BTreeMap::new();
        for (key, raw) in self.files {
            let file_path = files_path.key(&key);
            let span = raw.span();
            let location: Option<ContainerPath> = v.parse_at(&file_path, &key, span.clone());
            if let Some(location) = &location
                && let Err(reason) =
                    location.check_placement(mount_paths.iter().map(String::as_str))
            {
                v.error(&file_path, span.clone(), reason);
                valid = false;
            }
            let file = raw
                .into_inner()
                .validate(v, &file_path, span.clone(), stack);
            match (location, file) {
                (Some(location), Some(file)) => {
                    files.insert(location, (file, span));
                }
                _ => valid = false,
            }
        }
        // A file cannot also be a directory that holds another one.
        for (location, (_, span)) in &files {
            if let Some(below) = files.keys().find(|other| location.is_ancestor_of(other)) {
                v.error(
                    &files_path.key(location.as_ref()),
                    span.clone(),
                    format!("`{location}` is a parent directory of the file `{below}`"),
                );
                valid = false;
            }
        }
        let files = files
            .into_iter()
            .map(|(location, (file, _))| (location, file))
            .collect();

        let mut depends_on = BTreeSet::new();
        let mut listed = BTreeSet::new();
        for (i, dep) in self.depends_on.iter().enumerate() {
            let dep_path = path.key("depends_on").index(i);
            let dep_name = dep.get_ref();
            // The duplicate check comes first, so a repeated unknown or self entry reports its
            // problem once and each later copy as a duplicate.
            let problem = if !listed.insert(dep_name) {
                Some(format!("`{dep_name}` is listed more than once"))
            } else if dep_name == context.key {
                Some("a service cannot depend on itself".to_owned())
            } else if !context.services.contains(dep_name) {
                Some(format!(
                    "unknown service `{dep_name}`: `depends_on` names services of this stack"
                ))
            } else {
                None
            };
            if let Some(problem) = problem {
                v.error(&dep_path, dep.span(), problem);
                valid = false;
            } else if let Ok(dep_name) = dep_name.parse::<Name>() {
                depends_on.insert(dep_name);
            } else {
                // The service key itself is invalid, and reported there.
                valid = false;
            }
        }

        let raw_route_count = self.routes.len();
        let mut routes = Vec::with_capacity(raw_route_count);
        for (i, route) in self.routes.into_iter().enumerate() {
            match route.validate(v, &path.key("routes").index(i)) {
                Some(route) => routes.push(route),
                None => valid = false,
            }
        }

        let health = match self.health {
            None => Some(None),
            // Some routes are invalid and already reported; port checks against the rest
            // would only add noise.
            Some(_) if routes.len() != raw_route_count => None,
            Some(health) => {
                let span = health.span();
                health
                    .into_inner()
                    .validate(v, &path.key("health"), span, &routes)
                    .map(Some)
            }
        };

        let stop_timeout = v
            .optional_duration(path, "stop_timeout", &self.stop_timeout)
            .map(|d| d.unwrap_or(DEFAULT_STOP_TIMEOUT));

        if let Some(user) = &self.user
            && !Self::is_user_spec(user.get_ref())
        {
            v.error(
                &path.key("user"),
                user.span(),
                format!("invalid user `{}`: expected `uid[:gid]`", user.get_ref()),
            );
            valid = false;
        }

        valid.then_some(())?;
        Some(Service {
            image: image?,
            depends_on,
            track,
            command: self.command,
            entrypoint: self.entrypoint,
            env,
            secrets,
            volumes,
            files,
            health: health?,
            update: self.update,
            stop_timeout: stop_timeout?,
            user: self.user.map(Spanned::into_inner),
            read_only: self.read_only,
            routes,
        })
    }

    /// An absolute container path with exactly one spelling, so duplicate mounts are caught
    /// here rather than by podman: no empty (`//`, trailing `/`), `.` or `..` segments. `:` and
    /// `,` are rejected because they separate volume options in podman's mount syntax.
    fn is_mount_path(target: &str) -> bool {
        target.strip_prefix('/').is_some_and(|rest| {
            rest.split('/')
                .all(|segment| !matches!(segment, "" | "." | ".."))
                && !rest.chars().any(|c| c == ':' || c == ',' || c.is_control())
        })
    }

    /// `^[A-Za-z_][A-Za-z0-9_]*$`
    fn is_env_name(name: &str) -> bool {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    /// `uid[:gid]`, each a numeric id or a user/group name.
    fn is_user_spec(user: &str) -> bool {
        let part = |p: &str| {
            let mut chars = p.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
        };
        match user.split_once(':') {
            Some((uid, gid)) => part(uid) && part(gid),
            None => part(user),
        }
    }

    /// What to tell the author of a `value` that fails [`Self::is_secret_ref`].
    fn secret_ref_error(value: &str, stack: Option<&Name>) -> String {
        format!(
            "invalid secret name `{value}`: expected `<stack>/<name>` with this stack's name, \
             e.g. \"{}/database-url\"",
            stack.map_or("<stack>", |s| s.as_ref()),
        )
    }

    /// `<stack>/<name>` where `<stack>` is this stack's name (when it is valid).
    fn is_secret_ref(value: &str, stack: Option<&Name>) -> bool {
        let Some((owner, name)) = value.split_once('/') else {
            return false;
        };
        stack.is_none_or(|stack| stack.as_ref() == owner)
            && !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    }
}

impl RawFile {
    fn validate(
        self,
        v: &mut Validator<'_>,
        path: &FieldPath,
        span: Range<usize>,
        stack: Option<&Name>,
    ) -> Option<ConfigFile> {
        let source = match (self.content, self.secret) {
            (Some(content), None) => {
                let len = content.get_ref().len();
                if len > ConfigFile::MAX_BYTES {
                    v.error(
                        &path.key("content"),
                        content.span(),
                        format!(
                            "the content is {len} bytes; a file holds at most {} KiB",
                            ConfigFile::MAX_BYTES / 1024
                        ),
                    );
                    None
                } else {
                    Some(FileSource::Inline(content.into_inner()))
                }
            }
            (None, Some(secret)) => {
                if RawService::is_secret_ref(secret.get_ref(), stack) {
                    Some(FileSource::Secret(secret.into_inner()))
                } else {
                    v.error(
                        &path.key("secret"),
                        secret.span(),
                        RawService::secret_ref_error(secret.get_ref(), stack),
                    );
                    None
                }
            }
            (Some(_), Some(secret)) => {
                v.error(
                    &path.key("secret"),
                    secret.span(),
                    "set exactly one of `content` or `secret`",
                );
                None
            }
            (None, None) => {
                v.error(
                    path,
                    span,
                    "a file needs `content` or `secret`, e.g. { content = \"…\" }",
                );
                None
            }
        };
        // No `?` above: the mode and owner are checked even when the source is wrong.
        let mode: Option<Option<FileMode>> = match &self.mode {
            Some(mode) => v.parse(&path.key("mode"), mode).map(Some),
            None => Some(None),
        };
        let owner: Option<Option<FileOwner>> = match &self.owner {
            Some(owner) => v.parse(&path.key("owner"), owner).map(Some),
            None => Some(None),
        };
        Some(ConfigFile {
            source: source?,
            mode: mode?,
            owner: owner?,
        })
    }
}

impl RawRoute {
    fn validate(self, v: &mut Validator<'_>, path: &FieldPath) -> Option<Route> {
        let host: Option<Host> = v.parse(&path.key("host"), &self.host);
        let prefix = match &self.prefix {
            Some(prefix) => v.parse(&path.key("prefix"), prefix),
            None => Some(NormalizedPrefix::root()),
        };
        let strip_prefix = self.strip_prefix.as_ref().is_some_and(|s| *s.get_ref());
        if let (Some(flag), Some(prefix)) = (&self.strip_prefix, &prefix)
            && *flag.get_ref()
            && prefix.is_root()
        {
            v.warn(
                &path.key("strip_prefix"),
                flag.span(),
                "`strip_prefix` has no effect on the root prefix `/`",
            );
        }
        let port = v.port(&path.key("port"), &self.port);
        let timeouts = self.timeouts.validate(v, &path.key("timeouts"));
        Some(Route {
            host: host?,
            prefix: prefix?,
            strip_prefix,
            port: port?,
            timeouts: timeouts?,
        })
    }
}

impl RawTimeouts {
    fn validate(self, v: &mut Validator<'_>, path: &FieldPath) -> Option<RouteTimeouts> {
        let connect = v.positive_duration(path, "connect", &self.connect);
        let read = v.positive_duration(path, "read", &self.read);
        let write = v.positive_duration(path, "write", &self.write);
        let idle = v.positive_duration(path, "idle", &self.idle);
        Some(RouteTimeouts {
            connect: connect?,
            read: read?,
            write: write?,
            idle: idle?,
        })
    }
}

impl RawHealth {
    fn validate(
        self,
        v: &mut Validator<'_>,
        path: &FieldPath,
        span: Range<usize>,
        routes: &[Route],
    ) -> Option<HealthCheck> {
        // Probes target a container port that has an allocated loopback port, i.e. a route port.
        let default_port = routes.first().map(|r| r.port);
        let probe_port = |v: &mut Validator<'_>, port_path: FieldPath, port: &Option<Int>| {
            let Some(port) = port else {
                return default_port;
            };
            let value = v.port(&port_path, port)?;
            if routes.iter().any(|r| r.port == value) {
                Some(value)
            } else {
                v.error(
                    &port_path,
                    port.span(),
                    format!(
                        "health port {value} is not the container port of any route of this \
                         service; probes run against the route's loopback port"
                    ),
                );
                None
            }
        };

        let routed = !routes.is_empty();
        // ADR-0056 Decision 4: a service without routes has no loopback port, so `exec` is the
        // only probe it can declare.
        let needs_route = |v: &mut Validator<'_>, key: &str, span: Range<usize>| -> Option<Probe> {
            v.error(
                &path.key(key),
                span,
                format!(
                    "`{key}` probes need a route; a service without routes can only use `exec`, \
                     e.g. exec = [\"pg_isready\", \"-U\", \"app\"]"
                ),
            );
            None
        };
        let probe = match (self.http, self.tcp, self.exec) {
            (Some(http), None, None) if !routed => needs_route(v, "http", http.span()),
            (None, Some(tcp), None) if !routed => needs_route(v, "tcp", tcp.span()),
            (Some(http), None, None) => {
                let http_path = path.key("http");
                let http = http.into_inner();
                let probe_path = match &http.path {
                    Some(p) if !p.get_ref().starts_with('/') => {
                        v.error(
                            &http_path.key("path"),
                            p.span(),
                            format!("invalid health path `{}`: must start with `/`", p.get_ref()),
                        );
                        None
                    }
                    // The path goes verbatim into the probe's HTTP request line.
                    Some(p) if !p.get_ref().bytes().all(|b| b.is_ascii_graphic()) => {
                        v.error(
                            &http_path.key("path"),
                            p.span(),
                            format!(
                                "invalid health path `{}`: spaces, control and non-ASCII \
                                 characters must be percent-encoded",
                                p.get_ref().escape_debug()
                            ),
                        );
                        None
                    }
                    Some(p) => Some(p.get_ref().clone()),
                    None => Some(DEFAULT_HEALTH_PATH.to_owned()),
                };
                let port = probe_port(v, http_path.key("port"), &http.port);
                let expect_status = match &http.expect_status {
                    Some(s) => v.parse(&http_path.key("expect_status"), s),
                    None => Some(ExpectStatus::default()),
                };
                // No `?` here: that would skip validating the timings below.
                match (probe_path, port, expect_status) {
                    (Some(path), Some(port), Some(expect_status)) => Some(Probe::Http {
                        path,
                        port,
                        expect_status,
                    }),
                    _ => None,
                }
            }
            (None, Some(tcp), None) => {
                let port = probe_port(v, path.key("tcp").key("port"), &tcp.get_ref().port);
                port.map(|port| Probe::Tcp { port })
            }
            (None, None, Some(exec)) => Self::exec_probe(v, &path.key("exec"), exec),
            (None, None, None) => match default_port {
                Some(port) => Some(Probe::Http {
                    path: DEFAULT_HEALTH_PATH.to_owned(),
                    port,
                    expect_status: ExpectStatus::default(),
                }),
                None => {
                    v.error(
                        path,
                        span,
                        "a service without routes needs `exec` in its `health` table; without \
                         `health` it is healthy after running for 5 s",
                    );
                    None
                }
            },
            (http, tcp, exec) => {
                let second = [
                    http.map(|p| ("http", p.span())),
                    tcp.map(|p| ("tcp", p.span())),
                    exec.map(|p| ("exec", p.span())),
                ]
                .into_iter()
                .flatten()
                .nth(1);
                if let Some((key, span)) = second {
                    v.error(
                        &path.key(key),
                        span,
                        "set at most one of `http`, `tcp` or `exec`",
                    );
                }
                None
            }
        };

        let interval = v.positive_duration(path, "interval", &self.interval);
        let timeout = v.positive_duration(path, "timeout", &self.timeout);
        let start_period = v.positive_duration(path, "start_period", &self.start_period);
        let healthy_after = match &self.healthy_after {
            None => Some(None),
            Some(n) => match u32::try_from(*n.get_ref()) {
                Ok(n) if n > 0 => Some(Some(n)),
                _ => {
                    v.error(
                        &path.key("healthy_after"),
                        n.span(),
                        "must be a positive integer",
                    );
                    None
                }
            },
        };

        Some(HealthCheck {
            probe: probe?,
            interval: interval?,
            timeout: timeout?,
            healthy_after: healthy_after?,
            start_period: start_period?,
        })
    }

    /// A non-empty argv whose first element, the program, is not blank.
    fn exec_probe(
        v: &mut Validator<'_>,
        path: &FieldPath,
        exec: Spanned<Vec<Str>>,
    ) -> Option<Probe> {
        let span = exec.span();
        let command = exec.into_inner();
        let Some(program) = command.first() else {
            v.error(
                path,
                span,
                "must name the command and its arguments, e.g. \
                 [\"pg_isready\", \"-U\", \"app\"]",
            );
            return None;
        };
        let mut valid = true;
        let program_text = program.get_ref();
        if program_text.trim().is_empty() {
            v.error(
                &path.index(0),
                program.span(),
                "the command must not be empty",
            );
            valid = false;
        } else if program_text.trim() != program_text.as_str() {
            v.error(
                &path.index(0),
                program.span(),
                "the command must not start or end with whitespace: \
                 it is run without a shell",
            );
            valid = false;
        }
        for (i, arg) in command.iter().enumerate() {
            if arg.get_ref().contains('\0') {
                v.error(
                    &path.index(i),
                    arg.span(),
                    "must not contain a NUL character",
                );
                valid = false;
            }
        }
        valid.then(|| Probe::Exec {
            command: command.into_iter().map(Spanned::into_inner).collect(),
        })
    }
}
