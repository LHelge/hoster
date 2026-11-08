//! Stack → Secrets (ADR-0029, ADR-0049): the stack's secret names with their last change and
//! the services that reference them; set, replace and delete. Values are write-only: no page,
//! partial or error message ever contains one.
//!
//! Saving is two steps without the value ever travelling back to the browser. The first post of
//! the form answers with the services that reference the secret and will be redeployed, and a
//! confirm button bound to the same form (`form="secret-form"`) together with a hidden
//! `confirm` field holding the reviewed name. The second post carries the value from the
//! browser's own input again, and the secret is stored and those services redeployed only when
//! `confirm` names the secret being saved. Deleting a referenced secret needs its name typed.

use askama::Template;
use axum::Extension;
use axum::extract::{Form, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{DeploymentView, Outcome, StackView, Stacks, When};
use crate::orchestrator::{SecretChange, StackSecret};
use crate::secrets::SecretValue;
use crate::web::pages::{Layout, Page, Section, Shell};

/// The set form: a name within the stack, the value, and the name confirmed after review.
#[derive(Deserialize)]
pub struct SecretForm {
    #[serde(default)]
    name: String,
    value: SecretValue,
    /// The name the confirmation was shown for; absent on the first post.
    confirm: Option<String>,
}

/// The delete form of one secret.
#[derive(Deserialize)]
pub struct DeleteSecretForm {
    #[serde(default)]
    name: String,
    /// The secret's name, typed again; needed when services reference it.
    #[serde(default)]
    confirm: String,
}

impl SecretForm {
    /// One segment as stack files reference it: `[A-Za-z0-9._-]`, not `.` or `..`.
    fn valid_name(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && name.len() <= 200
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    }
}

impl Stacks {
    /// `GET /stacks/{name}/secrets`: the stack's secrets and the form to set one.
    pub async fn secrets_page(
        Extension(stacks): Extension<Stacks>,
        shell: Shell,
        Path(name): Path<String>,
    ) -> Response {
        let layout = shell.layout(Section::Stacks);
        match stacks.0.secrets(&name).await {
            Ok(secrets) => Page(SecretsPage {
                layout,
                stack_url: StackView::url(&name),
                action: SecretsPage::url(&name),
                secrets: secrets.iter().map(SecretView::from).collect(),
                stack: name,
            })
            .into_response(),
            Err(err) => Outcome::from(err).page(layout),
        }
    }

    /// `POST /stacks/{name}/secrets`: the confirmation listing the services that will be
    /// redeployed, or, once confirmed, stores the secret and redeploys them (ADR-0049).
    pub async fn save_secret(
        Extension(stacks): Extension<Stacks>,
        Path(stack): Path<String>,
        Form(form): Form<SecretForm>,
    ) -> Response {
        let name = form.name.trim();
        if !SecretForm::valid_name(name) {
            return Outcome::error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Enter a name of letters, digits, `.`, `_` and `-`, such as db-url.",
            )
            .into_response();
        }
        let value = form.value.normalized();
        if value.is_empty() {
            return Outcome::error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Enter the secret's value.",
            )
            .into_response();
        }
        if form.confirm.as_deref().map(str::trim) != Some(name) {
            let secrets = match stacks.0.secrets(&stack).await {
                Ok(secrets) => secrets,
                Err(err) => return Outcome::from(err).into_response(),
            };
            let current = secrets.iter().find(|s| s.name == name);
            return Page(SecretConfirm {
                secret: format!("{stack}/{name}"),
                name: name.to_owned(),
                exists: current.is_some_and(|s| s.updated_at.is_some()),
                services: current.map(|s| s.services.clone()).unwrap_or_default(),
            })
            .into_response();
        }
        match stacks.0.set_secret(&stack, name, &value).await {
            Ok(change) => SecretsPage::outcome(&stack, "Saved", &change),
            Err(err) => Outcome::from(err).into_response(),
        }
    }

    /// `POST /stacks/{name}/secrets/delete`: deletes a secret and redeploys the services that
    /// reference it; those need the name typed as confirmation.
    pub async fn delete_secret(
        Extension(stacks): Extension<Stacks>,
        Path(stack): Path<String>,
        Form(form): Form<DeleteSecretForm>,
    ) -> Response {
        let name = form.name.trim();
        let secrets = match stacks.0.secrets(&stack).await {
            Ok(secrets) => secrets,
            Err(err) => return Outcome::from(err).into_response(),
        };
        let Some(current) = secrets
            .iter()
            .find(|s| s.name == name && s.updated_at.is_some())
        else {
            return Outcome::error(
                StatusCode::NOT_FOUND,
                format!("{stack} has no secret called {name}."),
            )
            .into_response();
        };
        if !current.services.is_empty() && form.confirm.trim() != name {
            return Outcome::error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "{stack}/{name} is referenced by {}. Deleting it redeploys them, and those \
                     deploys fail until the secret is set again or the stack stops referencing \
                     it; their current containers keep running. Type {name} to confirm.",
                    current.services.join(", ")
                ),
            )
            .into_response();
        }
        match stacks.0.delete_secret(&stack, name).await {
            Ok(change) => SecretsPage::outcome(&stack, "Deleted", &change),
            Err(err) => Outcome::from(err).into_response(),
        }
    }
}

/// One secret as the page lists it.
#[derive(Debug)]
struct SecretView {
    name: String,
    /// The last change, or `None` when referenced but not set.
    changed_at: Option<String>,
    services: Vec<String>,
}

impl From<&StackSecret> for SecretView {
    fn from(secret: &StackSecret) -> Self {
        Self {
            name: secret.name.clone(),
            changed_at: secret.updated_at.map(|t| When(t).to_string()),
            services: secret.services.clone(),
        }
    }
}

/// `GET /stacks/{name}/secrets`.
#[derive(Template)]
#[template(path = "stack_secrets.html")]
pub struct SecretsPage {
    layout: Layout,
    stack: String,
    stack_url: String,
    /// Where the set form posts; the delete forms post to `<action>/delete`.
    action: String,
    secrets: Vec<SecretView>,
}

impl SecretsPage {
    fn url(stack: &str) -> String {
        format!("{}/secrets", StackView::url(stack))
    }

    /// What saving or deleting led to: the deployment that redeploys the referencing
    /// services, or that nothing needed it.
    fn outcome(stack: &str, done: &str, change: &SecretChange) -> Response {
        let secret = &change.secret;
        match &change.deployment {
            Some(deployment) => {
                let url = DeploymentView::url(stack, deployment.deployment);
                let mut response = Outcome::success(format!(
                    "{done} {secret}; deployment {} redeploys {} ({}).",
                    deployment.deployment,
                    change.services.join(", "),
                    deployment.state,
                ))
                .with_link(url.clone(), "Follow the deployment")
                .into_response();
                if let Ok(value) = url.parse() {
                    response.headers_mut().insert("hx-redirect", value);
                }
                response
            }
            None if !change.existed && done == "Deleted" => {
                Outcome::error(StatusCode::NOT_FOUND, format!("{secret} is not set."))
                    .into_response()
            }
            None => Outcome::success(format!(
                "{done} {secret}. No service references it, so nothing was redeployed."
            ))
            .with_link(Self::url(stack), "Reload the secrets")
            .into_response(),
        }
    }
}

/// The confirmation of a save: the services that will be redeployed.
#[derive(Template)]
#[template(path = "secret_confirm.html")]
pub struct SecretConfirm {
    secret: String,
    name: String,
    exists: bool,
    services: Vec<String>,
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
