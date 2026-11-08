//! Settings → Registries (ADR-0025): one pull credential per registry host, kept in the secret
//! store (ADR-0029). The token is write-only: the page lists hosts, usernames and when each was
//! last changed. "Test" makes the token request the registry's challenge asks for, with the
//! stored credential or anonymously, and reports the error class (401, 429, network) on
//! failure. Registry credentials are hoster's own: changing one redeploys nothing.

use std::sync::Arc;

use askama::Template;
use axum::Extension;
use axum::extract::Form;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::pages::{Layout, Page, Section, Shell};
use super::stacks::{Outcome, When};
use crate::registry::{
    CredentialError, RegistryCheck, RegistryClient, RegistryCredential, RegistryEntry,
    RegistryError, RegistryHost, StoredCredentials,
};
use crate::secrets::SecretValue;

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
    entries: Vec<EntryView>,
}

#[cfg(test)]
pub(super) mod tests;
