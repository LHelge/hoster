//! Pass 1 (strict serde into span-carrying types) and pass 2 (semantic validation).
//!
//! Every raw type has a `validate` method that reports problems to a [`Validator`] and returns
//! the model value when its own part was valid, so one run reports all errors.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use toml::Spanned;

use super::{
    DEFAULT_DRAIN_TIMEOUT, DEFAULT_HEALTH_PATH, DEFAULT_STOP_TIMEOUT, DeployAuth, Diagnostic,
    ExpectStatus, FieldPath, HealthCheck, Host, ImageRef, Location, Name, NormalizedPrefix,
    ParsedStack, Probe, Route, RouteTimeouts, SUPPORTED_VERSION, Service, Stack, StackError,
    UpdateStrategy,
};

type Str = Spanned<String>;
type Int = Spanned<i64>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawStack {
    version: Int,
    name: Str,
    #[serde(default)]
    drain_timeout: Option<Str>,
    #[serde(default)]
    deploy: Option<RawDeploy>,
    services: Spanned<BTreeMap<String, Spanned<RawService>>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDeploy {
    repository_id: Int,
    #[serde(default)]
    refs: Option<Spanned<Vec<Str>>>,
    #[serde(default)]
    environment: Option<Str>,
    #[serde(default)]
    job_workflow_ref: Option<Str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    image: Str,
    #[serde(default)]
    track: Option<Str>,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    entrypoint: Option<Vec<String>>,
    #[serde(default)]
    env: BTreeMap<String, Str>,
    #[serde(default)]
    secrets: BTreeMap<String, Str>,
    #[serde(default)]
    volumes: BTreeMap<String, Str>,
    #[serde(default)]
    health: Option<Spanned<RawHealth>>,
    #[serde(default)]
    update: UpdateStrategy,
    #[serde(default)]
    stop_timeout: Option<Str>,
    #[serde(default)]
    user: Option<Str>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    routes: Vec<RawRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    host: Str,
    #[serde(default)]
    prefix: Option<Str>,
    #[serde(default)]
    strip_prefix: Option<Spanned<bool>>,
    port: Int,
    #[serde(default)]
    timeouts: RawTimeouts,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTimeouts {
    #[serde(default)]
    connect: Option<Str>,
    #[serde(default)]
    read: Option<Str>,
    #[serde(default)]
    write: Option<Str>,
    #[serde(default)]
    idle: Option<Str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHealth {
    #[serde(default)]
    http: Option<Spanned<RawHttpProbe>>,
    #[serde(default)]
    tcp: Option<Spanned<RawTcpProbe>>,
    #[serde(default)]
    interval: Option<Str>,
    #[serde(default)]
    timeout: Option<Str>,
    #[serde(default)]
    healthy_after: Option<Int>,
    #[serde(default)]
    start_period: Option<Str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHttpProbe {
    #[serde(default)]
    path: Option<Str>,
    #[serde(default)]
    port: Option<Int>,
    #[serde(default)]
    expect_status: Option<Str>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTcpProbe {
    #[serde(default)]
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
        let mut services = BTreeMap::new();
        let mut all_valid = true;
        for (key, raw) in self.services.into_inner() {
            let path = services_path.key(&key);
            let service_name: Option<Name> = v.parse_at(&path, &key, raw.span());
            let service = raw.into_inner().validate(v, &path, name.as_ref());
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
        stack: Option<&Name>,
    ) -> Option<Service> {
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
                    format!(
                        "invalid secret name `{}`: expected `<stack>/<name>` with this stack's \
                         name, e.g. \"{}/database-url\"",
                        value.get_ref(),
                        stack.map_or("<stack>", |s| s.as_ref()),
                    ),
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
            Some(health) if raw_route_count == 0 => {
                v.error(
                    &path.key("health"),
                    health.span(),
                    "`health` is not allowed on a service without routes; such services are \
                     healthy after running for 5 s",
                );
                None
            }
            // Some routes are invalid and already reported; port checks against the rest
            // would only add noise.
            Some(_) if routes.len() != raw_route_count => None,
            Some(health) => health
                .into_inner()
                .validate(v, &path.key("health"), &routes)
                .map(Some),
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
            track,
            command: self.command,
            entrypoint: self.entrypoint,
            env,
            secrets,
            volumes,
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

        let probe = match (self.http, self.tcp) {
            (Some(http), None) => {
                let http_path = path.key("http");
                let http = http.into_inner();
                let probe_path = match &http.path {
                    Some(p) if p.get_ref().starts_with('/') && !p.get_ref().contains(' ') => {
                        Some(p.get_ref().clone())
                    }
                    Some(p) => {
                        v.error(
                            &http_path.key("path"),
                            p.span(),
                            format!("invalid health path `{}`: must start with `/`", p.get_ref()),
                        );
                        None
                    }
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
            (None, Some(tcp)) => {
                let port = probe_port(v, path.key("tcp").key("port"), &tcp.get_ref().port);
                port.map(|port| Probe::Tcp { port })
            }
            (Some(http), Some(_)) => {
                v.error(
                    path,
                    http.span(),
                    "set exactly one of `http` or `tcp`, not both",
                );
                None
            }
            (None, None) => default_port.map(|port| Probe::Http {
                path: DEFAULT_HEALTH_PATH.to_owned(),
                port,
                expect_status: ExpectStatus::default(),
            }),
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
}
