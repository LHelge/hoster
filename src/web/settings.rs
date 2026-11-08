//! Settings: a grouped sub-nav over sections that are each their own URL, `/settings/<part>`
//! (epic 8bf). Only Registries is editable. The others are read-only views of the configuration
//! hoster runs with ([`EffectiveConfig`], see `effective.rs`), because the configuration is
//! `hoster.toml` plus `HOSTER_*` (ADR-0024) and editing it in the UI is not adopted.
//!
//! Settings → Registries (ADR-0025): one pull credential per registry host, kept in the secret
//! store (ADR-0029). The token is write-only: the page lists hosts, usernames and when each was
//! last changed. "Test" makes the token request the registry's challenge asks for, with the
//! stored credential or anonymously, and reports the error class (401, 429, network) on
//! failure. Registry credentials are hoster's own: changing one redeploys nothing.

mod effective;

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use askama::Template;
use axum::Extension;
use axum::extract::{Form, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

pub use self::effective::EffectiveConfig;
use self::effective::{ConfigSection, Shown};
use super::certificates::CertificateControl;
use super::pages::{Layout, Page, Section, Shell};
use super::stacks::{Outcome, When};
use crate::registry::{
    CredentialError, RegistryCheck, RegistryClient, RegistryCredential, RegistryEntry,
    RegistryError, RegistryHost, StoredCredentials,
};
use crate::secrets::SecretValue;

/// A section of Settings, at `/settings/<slug>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPart {
    General,
    Registries,
    Webhook,
    Proxy,
    Certificates,
}

impl SettingsPart {
    /// The sub-nav's groups, in order.
    const GROUPS: [(&str, &[SettingsPart]); 3] = [
        ("instance", &[SettingsPart::General]),
        ("deploy", &[SettingsPart::Registries, SettingsPart::Webhook]),
        (
            "network",
            &[SettingsPart::Proxy, SettingsPart::Certificates],
        ),
    ];

    /// The first section, where `/settings` leads.
    const FIRST: SettingsPart = SettingsPart::General;

    /// The URL segment.
    pub fn slug(self) -> &'static str {
        match self {
            SettingsPart::General => "general",
            SettingsPart::Registries => "registries",
            SettingsPart::Webhook => "webhook",
            SettingsPart::Proxy => "proxy",
            SettingsPart::Certificates => "certificates",
        }
    }

    pub fn href(self) -> String {
        format!("/settings/{}", self.slug())
    }
}

/// The sub-nav's name of the section.
impl fmt::Display for SettingsPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SettingsPart::General => "general",
            SettingsPart::Registries => "registries",
            SettingsPart::Webhook => "deploy webhook",
            SettingsPart::Proxy => "reverse proxy",
            SettingsPart::Certificates => "certificates",
        })
    }
}

impl FromStr for SettingsPart {
    type Err = ();

    fn from_str(slug: &str) -> Result<Self, Self::Err> {
        SettingsPart::GROUPS
            .iter()
            .flat_map(|(_, parts)| parts.iter().copied())
            .find(|part| part.slug() == slug)
            .ok_or(())
    }
}

/// The Settings sub-nav of a page: its groups, the current section marked.
pub struct SettingsNav {
    pub current: SettingsPart,
    /// How many registry credentials are stored, shown next to Registries; `None` if unknown.
    registries: Option<usize>,
}

/// A group of the sub-nav.
pub struct NavGroup {
    pub label: &'static str,
    pub items: Vec<NavLink>,
}

/// A link of the sub-nav.
pub struct NavLink {
    pub href: String,
    pub label: SettingsPart,
    pub current: bool,
    pub note: Option<String>,
}

impl SettingsNav {
    pub fn groups(&self) -> Vec<NavGroup> {
        SettingsPart::GROUPS
            .iter()
            .map(|(label, parts)| NavGroup {
                label,
                items: parts
                    .iter()
                    .map(|&part| NavLink {
                        href: part.href(),
                        label: part,
                        current: part == self.current,
                        note: match part {
                            SettingsPart::Registries => self.registries.map(|n| n.to_string()),
                            _ => None,
                        },
                    })
                    .collect(),
            })
            .collect()
    }
}

/// The read-only sections' handlers: the effective configuration, the certificate overview
/// (for the public addresses the DNS check resolved) and the registries (for the sub-nav's
/// count), shared as a request extension.
#[derive(Clone)]
pub struct Settings {
    config: Arc<EffectiveConfig>,
    certificates: Arc<dyn CertificateControl>,
    registries: Arc<dyn RegistryControl>,
}

impl Settings {
    pub fn new(
        config: EffectiveConfig,
        certificates: Arc<dyn CertificateControl>,
        registries: Arc<dyn RegistryControl>,
    ) -> Self {
        Self {
            config: Arc::new(config),
            certificates,
            registries,
        }
    }

    /// `GET /settings`: the first section.
    pub async fn index() -> Redirect {
        Redirect::to(&SettingsPart::FIRST.href())
    }

    /// `GET /settings/{part}`: a read-only section. Registries has its own route.
    pub async fn section(
        Extension(settings): Extension<Settings>,
        Path(slug): Path<String>,
        shell: Shell,
    ) -> Response {
        let Ok(current) = slug.parse::<SettingsPart>() else {
            return (StatusCode::NOT_FOUND, "not found\n").into_response();
        };
        let config = &settings.config;
        let section = match current {
            SettingsPart::General => config.general(&settings.certificates.overview().await.public),
            SettingsPart::Webhook => config.webhook(),
            SettingsPart::Proxy => config.proxy(),
            SettingsPart::Certificates => config.certificates(),
            SettingsPart::Registries => return Redirect::to(Registries::URL).into_response(),
        };
        let registries = settings
            .registries
            .list()
            .await
            .inspect_err(|err| tracing::warn!(error = %err, "counting the registries failed"))
            .ok()
            .map(|entries| entries.len());
        Page(SettingsPage {
            layout: shell.layout(Section::Settings),
            nav: SettingsNav {
                current,
                registries,
            },
            section,
            file: config.file(),
        })
        .into_response()
    }
}

/// `GET /settings/{part}` for a read-only section.
#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsPage {
    layout: Layout,
    nav: SettingsNav,
    section: ConfigSection,
    /// The `hoster.toml` hoster read, for the footer.
    file: String,
}

/// What the registries page needs. Dyn-compatible, so tests use a fake.
#[async_trait::async_trait]
pub trait RegistryControl: Send + Sync + 'static {
    /// Every stored credential, without its token.
    async fn list(&self) -> Result<Vec<RegistryEntry>, CredentialError>;

    /// Stores (or replaces) the credential for `host`.
    async fn set(
        &self,
        host: &RegistryHost,
        credential: &RegistryCredential,
    ) -> Result<(), CredentialError>;

    /// Removes the credential for `host`; whether there was one.
    async fn remove(&self, host: &RegistryHost) -> Result<bool, CredentialError>;

    /// Tests access to `host`; see [`RegistryClient::check`].
    async fn check(&self, host: &RegistryHost) -> Result<RegistryCheck, RegistryError>;
}

/// The stored credentials and the registry client that uses them (production).
#[derive(Debug, Clone)]
pub struct RegistryAccess {
    pub credentials: StoredCredentials,
    pub client: Arc<RegistryClient>,
}

#[async_trait::async_trait]
impl RegistryControl for RegistryAccess {
    async fn list(&self) -> Result<Vec<RegistryEntry>, CredentialError> {
        self.credentials.list().await
    }

    async fn set(
        &self,
        host: &RegistryHost,
        credential: &RegistryCredential,
    ) -> Result<(), CredentialError> {
        self.credentials.set(host, credential).await?;
        // Tokens obtained with the old credential must not outlive it.
        self.client.forget_registry(host.as_ref());
        Ok(())
    }

    async fn remove(&self, host: &RegistryHost) -> Result<bool, CredentialError> {
        let removed = self.credentials.remove(host).await?;
        self.client.forget_registry(host.as_ref());
        Ok(removed)
    }

    async fn check(&self, host: &RegistryHost) -> Result<RegistryCheck, RegistryError> {
        self.client.check(host.as_ref()).await
    }
}

/// The registries page's handlers, sharing the control as a request extension.
#[derive(Clone)]
pub struct Registries(Arc<dyn RegistryControl>);

impl From<Arc<dyn RegistryControl>> for Registries {
    fn from(control: Arc<dyn RegistryControl>) -> Self {
        Self(control)
    }
}

/// The form to store a credential. The token goes straight into a [`SecretValue`].
#[derive(Deserialize)]
pub struct CredentialForm {
    #[serde(default)]
    host: String,
    #[serde(default)]
    username: String,
    token: SecretValue,
}

/// The forms that name just a host: delete and test.
#[derive(Deserialize)]
pub struct HostForm {
    #[serde(default)]
    host: String,
}

impl HostForm {
    fn host(raw: &str) -> Result<RegistryHost, Outcome> {
        raw.parse()
            .map_err(|reason: String| Outcome::error(StatusCode::UNPROCESSABLE_ENTITY, reason))
    }
}

impl Registries {
    const URL: &str = "/settings/registries";

    /// `GET /settings/registries`.
    pub async fn page(Extension(registries): Extension<Registries>, shell: Shell) -> Response {
        let layout = shell.layout(Section::Settings);
        match registries.0.list().await {
            Ok(entries) => Page(RegistriesPage {
                layout,
                nav: SettingsNav {
                    current: SettingsPart::Registries,
                    registries: Some(entries.len()),
                },
                entries: entries.iter().map(EntryView::from).collect(),
            })
            .into_response(),
            Err(err) => Self::failed(&err).page(layout),
        }
    }

    /// `POST /settings/registries`: stores the credential for a host. Redeploys nothing.
    pub async fn save(
        Extension(registries): Extension<Registries>,
        Form(form): Form<CredentialForm>,
    ) -> Response {
        let host = match HostForm::host(&form.host) {
            Ok(host) => host,
            Err(outcome) => return outcome.into_response(),
        };
        let username = form.username.trim();
        let token = form.token.expose().trim();
        if username.is_empty() || token.is_empty() {
            return Outcome::error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Enter both the username and the token.",
            )
            .into_response();
        }
        let credential = RegistryCredential::new(username, token);
        match registries.0.set(&host, &credential).await {
            Ok(()) => Outcome::success(format!(
                "Saved the credential for {host}. Pulls and tag lookups use it from now on; \
                 nothing was redeployed."
            ))
            .with_link(Self::URL.to_owned(), "Reload the registries")
            .into_response(),
            Err(err) => Self::failed(&err).into_response(),
        }
    }

    /// `POST /settings/registries/delete`.
    pub async fn delete(
        Extension(registries): Extension<Registries>,
        Form(form): Form<HostForm>,
    ) -> Response {
        let host = match HostForm::host(&form.host) {
            Ok(host) => host,
            Err(outcome) => return outcome.into_response(),
        };
        match registries.0.remove(&host).await {
            Ok(true) => Outcome::success(format!(
                "Deleted the credential for {host}; it is accessed anonymously from now on."
            ))
            .with_link(Self::URL.to_owned(), "Reload the registries")
            .into_response(),
            Ok(false) => Outcome::error(
                StatusCode::NOT_FOUND,
                format!("No credential is stored for {host}."),
            )
            .into_response(),
            Err(err) => Self::failed(&err).into_response(),
        }
    }

    /// `POST /settings/registries/test`: the result of a token request to the host.
    pub async fn test(
        Extension(registries): Extension<Registries>,
        Form(form): Form<HostForm>,
    ) -> Response {
        let host = match HostForm::host(&form.host) {
            Ok(host) => host,
            Err(outcome) => return outcome.into_response(),
        };
        match registries.0.check(&host).await {
            Ok(check) => Outcome::success(format!("{host}: {check}.")).into_response(),
            Err(err) => {
                tracing::info!(registry = %host, error = %err, "registry test failed");
                Outcome::error(
                    StatusCode::BAD_GATEWAY,
                    format!("{host}: the test failed ({}): {err}.", err.class()),
                )
                .into_response()
            }
        }
    }

    /// A credential-store failure. The messages name hosts, never tokens.
    fn failed(err: &CredentialError) -> Outcome {
        let status = match err {
            CredentialError::InvalidHost { .. } | CredentialError::Invalid { .. } => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            CredentialError::Malformed { .. } | CredentialError::Store { .. } => {
                tracing::error!(error = %err, "registry credential store failed");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Outcome::error(status, Outcome::sentence(&err.to_string()))
    }
}

/// One stored credential as the page lists it.
#[derive(Debug)]
struct EntryView {
    host: String,
    username: String,
    changed_at: String,
}

impl From<&RegistryEntry> for EntryView {
    fn from(entry: &RegistryEntry) -> Self {
        Self {
            host: entry.host.to_string(),
            username: entry.username.clone(),
            changed_at: When(entry.updated_at).to_string(),
        }
    }
}

/// `GET /settings/registries`.
#[derive(Template)]
#[template(path = "registries.html")]
pub struct RegistriesPage {
    layout: Layout,
    nav: SettingsNav,
    entries: Vec<EntryView>,
}

#[cfg(test)]
pub(super) mod tests;
