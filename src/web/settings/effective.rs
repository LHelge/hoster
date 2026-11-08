//! The read-only Settings sections: what hoster runs with, from `hoster.toml` and the `HOSTER_*`
//! environment (ADR-0024). Editing them in the UI is not adopted (epic 8bf), so each field shows
//! its effective value, which environment variable set it if one did, and a footer pointing at
//! the file.
//!
//! Only names, addresses, durations and paths are shown. Secrets live in files or systemd
//! credentials outside the configuration, and this module never reads them: the OAuth client
//! secret, the secret-store key and the ACME account key appear as their location only.

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::time::Duration;

use crate::acme::{AccountStore, Backoff};
use crate::config::{Config, ConfigSource};
use crate::stack::DEFAULT_DRAIN_TIMEOUT;
use crate::web::certificates::AcmeDirectory;
use crate::web::webhook::OidcVerifier;

/// The site guide on deploying from CI.
const CI_GUIDE: &str = "https://docs.hoster.lhelge.se/wiki/deploying-from-ci/";

/// The configuration hoster was started with, as the Settings sections show it.
#[derive(Debug, Clone)]
pub struct EffectiveConfig(Config);

impl From<&Config> for EffectiveConfig {
    fn from(config: &Config) -> Self {
        Self(config.clone())
    }
}

/// One read-only section: a head, its fields, and the "set in hoster.toml" footer.
#[derive(Debug)]
pub struct ConfigSection {
    pub title: &'static str,
    pub help: &'static str,
    pub fields: Vec<Field>,
}

/// One row: the label and help on the left, the value on the right.
#[derive(Debug)]
pub struct Field {
    pub label: &'static str,
    pub help: String,
    pub values: Vec<Shown>,
    /// The `HOSTER_*` variables that set it; empty when it comes from the file or a default.
    pub env: Vec<String>,
}

impl Field {
    fn new(label: &'static str, help: impl Into<String>) -> Self {
        Self {
            label,
            help: help.into(),
            values: Vec::new(),
            env: Vec::new(),
        }
    }

    /// The same field marked with the variables that override `key`.
    fn key(mut self, source: &ConfigSource, key: &str) -> Self {
        self.env = source.overrides(key);
        self
    }

    /// The same field, also marked with the variables that override `key`.
    fn key_also(mut self, source: &ConfigSource, key: &str) -> Self {
        self.env.extend(source.overrides(key));
        self
    }

    /// A machine value (monospace).
    fn value(mut self, text: impl Into<String>) -> Self {
        self.values.push(Shown::Value {
            text: text.into(),
            note: None,
        });
        self
    }

    /// A machine value with a muted note after it.
    fn noted(mut self, text: impl Into<String>, note: impl Into<String>) -> Self {
        self.values.push(Shown::Value {
            text: text.into(),
            note: Some(note.into()),
        });
        self
    }

    /// A sentence of prose.
    fn text(mut self, text: impl Into<String>) -> Self {
        self.values.push(Shown::Text(text.into()));
        self
    }

    fn shown(mut self, shown: Shown) -> Self {
        self.values.push(shown);
        self
    }
}

/// What a field shows.
#[derive(Debug)]
pub enum Shown {
    /// A machine value in monospace, with an optional muted note.
    Value { text: String, note: Option<String> },
    /// Prose.
    Text(String),
    /// A link (to the site's guides).
    Link {
        href: &'static str,
        text: &'static str,
    },
    /// The options of a choice, the one in use marked, drawn like radio cards but read-only.
    Choices(Vec<Choice>),
}

/// One option of [`Shown::Choices`].
#[derive(Debug)]
pub struct Choice {
    pub label: &'static str,
    pub detail: String,
    pub on: bool,
}

/// A duration as hoster.toml writes it, in hours, minutes, seconds and milliseconds: `5m`,
/// `24h`, `1h 30m`, `1s 500ms`.
struct Span(Duration);

impl std::fmt::Display for Span {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secs = self.0.as_secs();
        let millis = u64::from(self.0.subsec_millis());
        if secs == 0 {
            return write!(f, "{millis}ms");
        }
        let parts = [
            (secs / 3600, "h"),
            (secs / 60 % 60, "m"),
            (secs % 60, "s"),
            (millis, "ms"),
        ];
        let mut first = true;
        for (n, unit) in parts.into_iter().filter(|(n, _)| *n > 0) {
            if !first {
                f.write_str(" ")?;
            }
            write!(f, "{n}{unit}")?;
            first = false;
        }
        Ok(())
    }
}

/// An address's family, as a note: `ipv4` or `ipv6`.
struct Family<'a>(&'a IpAddr);

impl std::fmt::Display for Family<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_ipv4() { "ipv4" } else { "ipv6" })
    }
}

impl EffectiveConfig {
    fn source(&self) -> &ConfigSource {
        &self.0.source
    }

    /// The `hoster.toml` hoster read.
    pub fn file(&self) -> String {
        self.0.source.file.display().to_string()
    }

    /// General: who and where. `resolved` are the public addresses the DNS check last resolved
    /// from the admin host, used when `acme.public_addresses` is empty.
    pub fn general(&self, resolved: &BTreeSet<IpAddr>) -> ConfigSection {
        let config = &self.0;
        let source = self.source();
        let admin_host = config.admin_host.to_string();

        let mut public = Field::new(
            "Public addresses",
            "The advisory DNS check compares each host's A/AAAA records with these (ADR-0051).",
        )
        .key(source, "acme.public_addresses");
        if !config.acme.public_addresses.is_empty() {
            for addr in &config.acme.public_addresses {
                public = public.noted(addr.to_string(), format!("{} · configured", Family(addr)));
            }
        } else if resolved.is_empty() {
            public = public.text(format!(
                "Not resolved yet: taken from the A/AAAA records of {admin_host}, re-resolved hourly."
            ));
        } else {
            for addr in resolved {
                public = public.noted(
                    addr.to_string(),
                    format!("{} · resolved from {admin_host}", Family(addr)),
                );
            }
        }

        let listener = match config.web.dev_listen {
            Some(addr) => Field::new(
                "Admin app listener",
                "Loopback TCP instead of the socket: for local development only.",
            )
            .value(format!("http://{addr}")),
            None => Field::new(
                "Admin app listener",
                "The proxy routes the admin host to this 0600 Unix socket (ADR-0031).",
            )
            .value(
                config
                    .state_dir
                    .join(Config::ADMIN_SOCKET_NAME)
                    .display()
                    .to_string(),
            ),
        }
        .key(source, "web.dev_listen");

        let podman = Field::new(
            "Podman socket",
            "The rootless podman API hoster drives (ADR-0002).",
        )
        .key(source, "podman_socket");
        let podman = match &config.podman_socket {
            Some(path) => podman.value(path.display().to_string()),
            None => podman
                .value("$XDG_RUNTIME_DIR/podman/podman.sock")
                .text("The hoster user's own socket (the default)."),
        };

        let env = source.env_vars();
        let overrides = Field::new(
            "Environment overrides",
            "HOSTER_* variables override the file; HOSTER_ADMIN_GITHUB is only read from the \
             environment. Names only: values are not shown.",
        );
        let overrides = if env.is_empty() {
            overrides.text("None.")
        } else {
            env.into_iter().fold(overrides, Field::value)
        };

        ConfigSection {
            title: "General",
            help: "Who this instance answers to, and where it keeps its state.",
            fields: vec![
                Field::new(
                    "Admin hostname",
                    "Serves this UI and the deploy webhook. No stack may route it (ADR-0031).",
                )
                .key(source, "admin_host")
                .value(admin_host.clone()),
                Field::new(
                    "Admin GitHub handle",
                    "The one account that may sign in (ADR-0021).",
                )
                .key(source, "admin_github")
                .value(format!("@{}", config.admin_github)),
                public,
                Field::new(
                    "State directory",
                    "SQLite, certificates, the ACME account and the admin socket.",
                )
                .key(source, "state_dir")
                .value(config.state_dir.display().to_string()),
                Field::new(
                    "Secret-store key",
                    "Encrypts the stacks' secrets and registry tokens (ADR-0029). The systemd \
                     credential secret_key takes its place when set. The key is never shown.",
                )
                .key(source, "secret_key_file")
                .value(config.secret_key_file.display().to_string()),
                Field::new(
                    "GitHub OAuth App",
                    "The sign-in's client id and secret are read from these files or systemd \
                     credentials. The secret is never shown.",
                )
                .key(source, "web.github_client_id_file")
                .key_also(source, "web.github_client_secret_file")
                .noted(
                    Self::credential(
                        config.web.github_client_id_file.as_deref(),
                        "github_client_id",
                    ),
                    "client id",
                )
                .noted(
                    Self::credential(
                        config.web.github_client_secret_file.as_deref(),
                        "github_client_secret",
                    ),
                    "client secret",
                ),
                Field::new(
                    "Session idle timeout",
                    format!(
                        "A session ends after this long without a request, and {} after sign-in \
                         at the latest.",
                        Span(crate::config::WebConfig::SESSION_LIFETIME)
                    ),
                )
                .key(source, "web.session_idle_timeout")
                .value(Span(config.web.session_idle_timeout).to_string()),
                listener,
                podman,
                Field::new(
                    "Log format",
                    "hoster's own log lines on stderr; RUST_LOG sets the level.",
                )
                .key(source, "log.format")
                .value(config.log.format.to_string()),
                Field::new("Config file", "").value(self.file()),
                overrides,
            ],
        }
    }

    /// Where a credential is read from: its file, or the systemd credential `name`.
    fn credential(file: Option<&std::path::Path>, name: &str) -> String {
        match file {
            Some(path) => path.display().to_string(),
            None => format!("systemd credential {name}"),
        }
    }

    /// Deploy webhook: what CI calls and what its token must say (ADR-0042, ADR-0047).
    pub fn webhook(&self) -> ConfigSection {
        let config = &self.0;
        let source = self.source();
        let audience = Field::new(
            "Expected audience",
            "The aud claim to request with core.getIDToken(audience) in the workflow.",
        )
        .key(source, "webhook.audience");
        let default = format!("https://{}", config.admin_host);
        let audience = if config.webhook_audience() == default {
            audience.noted(default, "default: https://<admin_host>")
        } else {
            audience.value(config.webhook_audience())
        };
        ConfigSection {
            title: "Deploy webhook",
            help: "How CI deploys a new image digest. Authentication is GitHub Actions OIDC \
                   only: there is no shared secret to copy (ADR-0047).",
            fields: vec![
                Field::new(
                    "Webhook URL",
                    "POST here with Authorization: Bearer <OIDC token>; the body names each \
                     service's image digest.",
                )
                .value(format!("{}/hooks/deploy", config.admin_origin())),
                Field::new(
                    "Expected issuer",
                    "Tokens are verified against GitHub's published signing keys.",
                )
                .value(OidcVerifier::ISSUER),
                audience,
                Field::new(
                    "Who may deploy",
                    "Set per stack, in its stack file, not here.",
                )
                .text(
                    "A stack's [deploy] table names the repository_id allowed to deploy it, \
                     optionally narrowed by refs, environment and job_workflow_ref. A stack \
                     without one deploys from the UI only.",
                ),
                Field::new(
                    "Digest poller",
                    "Tracked tags are also polled; a digest that changed is deployed (ADR-0043).",
                )
                .key(source, "poller.interval")
                .noted(
                    Span(config.poller.interval).to_string(),
                    "between polls, plus jitter",
                ),
                Field::new("Guide", "The workflow to copy, step by step.").shown(Shown::Link {
                    href: CI_GUIDE,
                    text: "Deploying from CI",
                }),
            ],
        }
    }

    /// Reverse proxy: listeners, upstream defaults and the edge rules (ADR-0011, ADR-0032).
    pub fn proxy(&self) -> ConfigSection {
        let config = &self.0;
        let source = self.source();
        let proxy = &config.proxy;
        let timeouts = proxy.timeouts;
        ConfigSection {
            title: "Reverse proxy",
            help: "The embedded Pingora proxy in front of every stack (ADR-0008).",
            fields: vec![
                Field::new(
                    "HTTP listener",
                    "Answers ACME challenges and redirects known hosts to HTTPS.",
                )
                .key(source, "proxy.http_listen")
                .value(proxy.http_listen.to_string()),
                Field::new(
                    "HTTPS listener",
                    "TLS for every routed host and the admin host.",
                )
                .key(source, "proxy.https_listen")
                .value(proxy.https_listen.to_string()),
                Field::new(
                    "Worker threads",
                    "Per Pingora service; the CPU count by default.",
                )
                .key(source, "proxy.threads")
                .value(proxy.threads.to_string()),
                Field::new(
                    "Upstream port range",
                    "Loopback ports handed to routed services, round-robin (ADR-0026).",
                )
                .key(source, "upstream_ports")
                .noted(
                    config.upstream_ports.to_string(),
                    format!("{} ports", config.upstream_ports.len()),
                ),
                Field::new(
                    "Default timeouts",
                    "For routes that set none of their own (ADR-0011).",
                )
                .key(source, "proxy.timeouts")
                .noted(Span(timeouts.connect).to_string(), "connect")
                .noted(Span(timeouts.read).to_string(), "read")
                .noted(Span(timeouts.write).to_string(), "write")
                .noted(Span(timeouts.idle).to_string(), "idle"),
                Field::new(
                    "Drain timeout",
                    "How long old upstreams may finish in-flight requests after a swap. Set per \
                     stack with drain_timeout in its stack file, not in hoster.toml.",
                )
                .noted(Span(DEFAULT_DRAIN_TIMEOUT).to_string(), "default"),
                Field::new(
                    "Shutdown",
                    "When hoster stops: in-flight requests get the grace period, then the \
                     runtimes the shutdown timeout.",
                )
                .key(source, "proxy.grace_period")
                .key_also(source, "proxy.graceful_shutdown_timeout")
                .noted(Span(proxy.grace_period).to_string(), "grace period")
                .noted(
                    Span(proxy.graceful_shutdown_timeout).to_string(),
                    "shutdown timeout",
                ),
                Field::new("Edge behaviour", "Fixed rules, not settings (ADR-0032).")
                    .noted(
                        "no certificate",
                        "TLS for a host without one: the handshake fails",
                    )
                    .noted("421", "Host header differs from the TLS SNI")
                    .noted("404", "known host, no matching route prefix")
                    .noted("404", "plain HTTP for an unknown host; no redirect")
                    .noted("308", "plain HTTP for a known host, to https://")
                    .noted("502", "the upstream refused or reset the connection")
                    .noted(
                        "503",
                        "a stop-first service between containers, with Retry-After",
                    ),
            ],
        }
    }

    /// Certificates: the ACME account and the renewal rules (ADR-0015, ADR-0016, ADR-0036).
    pub fn certificates(&self) -> ConfigSection {
        let config = &self.0;
        let source = self.source();
        let acme = &config.acme;
        let directory = AcmeDirectory::from(acme.directory_url.as_str());
        let custom = match &directory {
            AcmeDirectory::Custom(url) => url.clone(),
            _ => "Any other ACME directory, such as a test CA".to_owned(),
        };
        let choices = vec![
            Choice {
                label: "Let's Encrypt",
                detail: AcmeDirectory::LetsEncrypt.url().to_owned(),
                on: directory == AcmeDirectory::LetsEncrypt,
            },
            Choice {
                label: "Let's Encrypt staging",
                detail: AcmeDirectory::LetsEncryptStaging.url().to_owned(),
                on: directory == AcmeDirectory::LetsEncryptStaging,
            },
            Choice {
                label: "Custom ACME directory",
                detail: custom,
                on: matches!(directory, AcmeDirectory::Custom(_)),
            },
        ];
        let field = Field::new(
            "Contact email",
            "Registered with the ACME account; the CA may send expiry warnings here.",
        )
        .key(source, "acme.contact");
        let contact = match &acme.contact {
            Some(contact) => field.value(contact.strip_prefix("mailto:").unwrap_or(contact)),
            None => field.text("None registered."),
        };
        let mut fields =
            vec![
            Field::new(
                "Certificate authority",
                "Use staging while testing: its rate limits are far higher, but browsers do not \
                 trust it. Changing the directory registers a new account.",
            )
            .key(source, "acme.directory_url")
            .shown(Shown::Choices(choices)),
            contact,
            Field::new(
                "Renewal",
                "Every wanted host is looked at hourly (ADR-0015).",
            )
            .text(
                "Renewed once less than a third of the certificate's lifetime remains, or \
                 earlier when the CA's renewal window (ARI) opens.",
            ),
            Field::new(
                "Retry backoff",
                "Per host, kept across restarts in the state store.",
            )
            .noted(
                format!("{} → {}", Span(Backoff::INITIAL), Span(Backoff::MAX)),
                "doubling after each failed attempt",
            ),
            Field::new(
                "Account key",
                "A 0600 file holding the ACME account's credentials; the key is never shown.",
            )
            .value(AccountStore::new(&config.state_dir).path().display().to_string()),
            Field::new("Challenge", "")
                .text("HTTP-01, answered on the HTTP listener."),
        ];
        if let Some(root) = &acme.root_certificate {
            fields.push(
                Field::new(
                    "Trusted root",
                    "The ACME directory is verified against this PEM instead of the platform \
                     roots (a test CA).",
                )
                .key(source, "acme.root_certificate")
                .value(root.display().to_string()),
            );
        }
        ConfigSection {
            title: "Certificates (ACME)",
            help: "One certificate per routed host, issued at apply and renewed in the \
                   background (ADR-0036).",
            fields,
        }
    }
}

#[cfg(test)]
impl EffectiveConfig {
    /// A configuration as a typical install has it, with secret files whose paths are shown
    /// but whose content must never be.
    pub fn sample() -> Self {
        use std::path::PathBuf;

        let home = PathBuf::from("/home/hoster");
        Self(Config {
            state_dir: home.join(".local/share/hoster"),
            secret_key_file: home.join(".config/hoster/secret.key"),
            admin_host: "admin.example.com".parse().unwrap(),
            admin_github: "octocat".parse().unwrap(),
            podman_socket: None,
            acme: crate::config::AcmeConfig {
                contact: Some("certs@example.com".into()),
                ..Default::default()
            },
            upstream_ports: Default::default(),
            proxy: Default::default(),
            poller: Default::default(),
            webhook: Default::default(),
            web: crate::config::WebConfig {
                github_client_secret_file: Some(home.join(".config/hoster/github-secret")),
                ..Default::default()
            },
            log: Default::default(),
            credentials: Default::default(),
            source: ConfigSource::new(
                home.join(".config/hoster/hoster.toml"),
                ["admin_github", "proxy.timeouts.read"],
            ),
        })
    }

    /// The same configuration with `edit` applied.
    pub fn with(mut self, edit: impl FnOnce(&mut Config)) -> Self {
        edit(&mut self.0);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(field: &Field) -> Vec<String> {
        field
            .values
            .iter()
            .map(|shown| match shown {
                Shown::Value { text, note } => match note {
                    Some(note) => format!("{text} ({note})"),
                    None => text.clone(),
                },
                Shown::Text(text) => text.clone(),
                Shown::Link { href, .. } => (*href).to_owned(),
                Shown::Choices(choices) => choices
                    .iter()
                    .map(|c| format!("{}{}", if c.on { "*" } else { "" }, c.label))
                    .collect::<Vec<_>>()
                    .join(", "),
            })
            .collect()
    }

    fn field<'a>(section: &'a ConfigSection, label: &str) -> &'a Field {
        section
            .fields
            .iter()
            .find(|f| f.label == label)
            .unwrap_or_else(|| panic!("no field {label}"))
    }

    #[test]
    fn public_addresses_say_where_they_came_from() {
        let config = EffectiveConfig::sample();
        let general = config.general(&BTreeSet::new());
        assert!(values(field(&general, "Public addresses"))[0].starts_with("Not resolved yet"));

        let resolved = ["192.0.2.1".parse().unwrap(), "2001:db8::1".parse().unwrap()].into();
        let general = config.general(&resolved);
        assert_eq!(
            values(field(&general, "Public addresses")),
            [
                "192.0.2.1 (ipv4 · resolved from admin.example.com)",
                "2001:db8::1 (ipv6 · resolved from admin.example.com)"
            ]
        );

        let config =
            config.with(|c| c.acme.public_addresses = vec!["203.0.113.4".parse().unwrap()]);
        let general = config.general(&resolved);
        assert_eq!(
            values(field(&general, "Public addresses")),
            ["203.0.113.4 (ipv4 · configured)"]
        );
    }

    #[test]
    fn fields_set_by_the_environment_name_their_variable() {
        let config = EffectiveConfig::sample();
        let general = config.general(&BTreeSet::new());
        assert_eq!(
            field(&general, "Admin GitHub handle").env,
            ["HOSTER_ADMIN_GITHUB"]
        );
        assert!(field(&general, "Admin hostname").env.is_empty());
        assert_eq!(
            values(field(&general, "Environment overrides")),
            ["HOSTER_ADMIN_GITHUB", "HOSTER_PROXY__TIMEOUTS__READ"]
        );
        assert_eq!(
            field(&config.proxy(), "Default timeouts").env,
            ["HOSTER_PROXY__TIMEOUTS__READ"]
        );
    }

    #[test]
    fn the_webhook_section_has_the_url_issuer_and_audience() {
        let webhook = EffectiveConfig::sample().webhook();
        assert_eq!(
            values(field(&webhook, "Webhook URL")),
            ["https://admin.example.com/hooks/deploy"]
        );
        assert_eq!(
            values(field(&webhook, "Expected issuer")),
            ["https://token.actions.githubusercontent.com"]
        );
        assert_eq!(
            values(field(&webhook, "Expected audience")),
            ["https://admin.example.com (default: https://<admin_host>)"]
        );
        assert_eq!(values(field(&webhook, "Guide")), [CI_GUIDE]);
    }

    #[test]
    fn the_certificate_authority_in_use_is_marked() {
        let config = EffectiveConfig::sample();
        assert_eq!(
            values(field(&config.certificates(), "Certificate authority")),
            ["*Let's Encrypt, Let's Encrypt staging, Custom ACME directory"]
        );
        let config = config.with(|c| c.acme.directory_url = "https://pebble:14000/dir".into());
        let section = config.certificates();
        let Shown::Choices(choices) = &field(&section, "Certificate authority").values[0] else {
            panic!("not choices");
        };
        assert!(choices[2].on);
        assert_eq!(choices[2].detail, "https://pebble:14000/dir");
        assert_eq!(
            values(field(&section, "Account key")),
            ["/home/hoster/.local/share/hoster/acme/account.json"]
        );
        assert_eq!(
            values(field(&section, "Retry backoff")),
            ["5m → 24h (doubling after each failed attempt)"]
        );
    }

    #[test]
    fn spans_read_like_the_config() {
        let span = |secs| Span(Duration::from_secs(secs)).to_string();
        assert_eq!(span(30), "30s");
        assert_eq!(span(300), "5m");
        assert_eq!(span(5400), "1h 30m");
        assert_eq!(span(86_400), "24h");
        assert_eq!(Span(Duration::from_millis(250)).to_string(), "250ms");
        assert_eq!(Span(Duration::from_millis(1500)).to_string(), "1s 500ms");
    }

    #[test]
    fn proxy_defaults_are_shown() {
        let proxy = EffectiveConfig::sample().proxy();
        assert_eq!(
            values(field(&proxy, "Upstream port range")),
            ["20000-29999 (10000 ports)"]
        );
        assert_eq!(values(field(&proxy, "Drain timeout")), ["30s (default)"]);
        assert!(
            values(field(&proxy, "Edge behaviour"))
                .iter()
                .any(|v| v.starts_with("421"))
        );
    }
}
