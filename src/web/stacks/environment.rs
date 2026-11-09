//! A stack's environment tab (ADR-0029, ADR-0049, epic 8bf, task awz): every environment
//! variable the active revision's stack file gives a service, and the stack's secrets, in one
//! table under the header and tab bar every stack page shares.
//!
//! Plain variables (`env`, plus the `PORT` hoster adds for a routed service, ADR-0051) show their
//! values: they are in the stack file anyway, and they change only through it, by applying a new
//! revision, so the tab has no edit for them. Secrets
//! (`secrets = { ENV = "<stack>/<name>" }`) show the secret's name in place of a value, never the
//! value: the store is write-only (ADR-0029), and no page, partial or error message here ever
//! contains one. A referenced secret that is not set is flagged; a stored one the active revision
//! does not reference is listed without a key or service, so it can still be deleted.
//!
//! Saving is two steps without the value ever travelling back to the browser. The first post of
//! the form answers with the services that reference the secret and will be redeployed, and a
//! confirm button bound to the same form (`form="secret-form"`) together with a hidden
//! `confirm` field holding the reviewed name. The second post carries the value from the
//! browser's own input again, and the secret is stored and those services redeployed only when
//! `confirm` names the secret being saved (ADR-0049). Deleting works the same way for a
//! referenced secret: the first post answers with the services it redeploys and a form that
//! needs the name typed; an unreferenced one deletes after a browser confirm.
//!
//! The tab lives at `/stacks/{name}/environment`; the earlier `/stacks/{name}/secrets` page
//! redirects there, and its form endpoints still accept posts.

use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use askama::Template;
use axum::Extension;
use axum::extract::{Form, Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::frame::{StackHeader, Tab};
use super::list::Ago;
use super::{DeploymentView, Outcome, StackView, Stacks, When};
use crate::orchestrator::{SecretChange, StackSecret, StackStatus};
use crate::secrets::SecretValue;
use crate::stack::Stack;
use crate::web::auth::Session;
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
    /// The secret's name, typed again; needed when services reference it. Absent on the first
    /// post, which answers with the form to type it in.
    confirm: Option<String>,
}

/// The query of the tab: `?set=<name>` fills the set form's name, as a row's "set" and
/// "replace" links do.
#[derive(Debug, Default, Deserialize)]
pub struct EnvironmentQuery {
    set: Option<String>,
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
    /// `GET /stacks/{name}/environment[?set=<secret>]`: the variables and secrets, and the form
    /// to set a secret; an unknown stack is a 404.
    pub async fn environment(
        Extension(stacks): Extension<Stacks>,
        shell: Shell,
        Path(name): Path<String>,
        Query(query): Query<EnvironmentQuery>,
    ) -> Response {
        let layout = shell.layout(Section::Stacks);
        let status = match stacks.find(&name).await {
            Ok(status) => status,
            Err(outcome) => return outcome.page(layout),
        };
        let secrets = match stacks.0.secrets(&name).await {
            Ok(secrets) => secrets,
            Err(err) => return Outcome::from(err).page(layout),
        };
        let prefill = query
            .set
            .map(|set| set.trim().to_owned())
            .filter(|set| SecretForm::valid_name(set))
            .unwrap_or_default();
        Page(EnvironmentPage {
            layout,
            header: stacks.header(&status, Tab::Environment).await,
            env: EnvTable::new(&status, &secrets, SystemTime::now()),
            action: EnvironmentPage::url(&name),
            names: secrets.into_iter().map(|s| s.name).collect(),
            prefill,
        })
        .into_response()
    }

    /// `GET /stacks/{name}/secrets`: the page the environment tab replaced.
    pub async fn secrets_moved(Path(name): Path<String>) -> Redirect {
        Redirect::permanent(&EnvironmentPage::url(&name))
    }

    /// `POST /stacks/{name}/environment`: the confirmation listing the services that will be
    /// redeployed, or, once confirmed, stores the secret and redeploys them (ADR-0049).
    pub async fn save_secret(
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
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
        match stacks
            .0
            .set_secret(&stack, name, &value, &session.login().into())
            .await
        {
            Ok(change) => EnvironmentPage::outcome(&stack, "Saved", &change),
            Err(err) => Outcome::from(err).into_response(),
        }
    }

    /// `POST /stacks/{name}/environment/delete`: deletes a secret and redeploys the services
    /// that reference it. Those need the name typed: the first post without `confirm` answers
    /// with the form to type it in.
    pub async fn delete_secret(
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
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
        if !current.services.is_empty() {
            let confirm = |error: Option<String>| DeleteConfirm {
                action: format!("{}/delete", EnvironmentPage::url(&stack)),
                secret: format!("{stack}/{name}"),
                name: name.to_owned(),
                services: current.services.clone(),
                error,
            };
            match form.confirm.as_deref().map(str::trim) {
                None => return Page(confirm(None)).into_response(),
                // The form again, so the name can be retyped, with why it was refused.
                Some(typed) if typed != name => {
                    let error = format!(
                        "{stack}/{name} is referenced by {}. Deleting it redeploys them, and \
                         those deploys fail until the secret is set again or the stack stops \
                         referencing it; their current containers keep running. Type {name} to \
                         confirm.",
                        current.services.join(", ")
                    );
                    return (StatusCode::UNPROCESSABLE_ENTITY, Page(confirm(Some(error))))
                        .into_response();
                }
                Some(_) => {}
            }
        }
        match stacks
            .0
            .delete_secret(&stack, name, &session.login().into())
            .await
        {
            Ok(change) => EnvironmentPage::outcome(&stack, "Deleted", &change),
            Err(err) => Outcome::from(err).into_response(),
        }
    }
}

/// A secret as its row shows it.
#[derive(Debug)]
struct SecretCell {
    /// The name within the stack: `db` for `notes/db`.
    name: String,
    /// When it was last set, as (how long ago, the time); `None` when it is not set.
    changed: Option<(String, String)>,
    /// Whether a service of the active revision references it, so deleting it redeploys.
    referenced: bool,
}

/// What a row sets: a plain variable's value, or a secret by name.
#[derive(Debug)]
enum EnvValue {
    /// A plain variable from the stack file's `env`.
    Var {
        value: String,
        /// Added by hoster rather than the stack file's `env`: `PORT` for a routed service
        /// with one container port (ADR-0051).
        injected: bool,
    },
    /// A secret from the store, referenced through `secrets` or only stored.
    Secret {
        /// The secret's full name `<stack>/<name>`.
        full: String,
        cell: SecretCell,
    },
}

impl EnvValue {
    /// What the value column shows: a variable's value or a secret's full name.
    fn text(&self) -> &str {
        match self {
            EnvValue::Var { value, .. } => value,
            EnvValue::Secret { full, .. } => full,
        }
    }

    fn secret(&self) -> Option<&SecretCell> {
        match self {
            EnvValue::Var { .. } => None,
            EnvValue::Secret { cell, .. } => Some(cell),
        }
    }

    fn injected(&self) -> bool {
        matches!(self, EnvValue::Var { injected: true, .. })
    }
}

/// The type pill: `var` or `secret`.
impl std::fmt::Display for EnvValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EnvValue::Var { .. } => "var",
            EnvValue::Secret { .. } => "secret",
        })
    }
}

/// One row: a variable of one service, or a stored secret nothing references.
#[derive(Debug)]
struct EnvRow {
    /// The variable's name; `None` for a stored secret no service references.
    key: Option<String>,
    /// The service it is set for; `None` for a stored secret no service references. Without a
    /// readable stack file, the services the store reports, joined.
    service: Option<String>,
    value: EnvValue,
}

/// The table of the tab.
#[derive(Debug)]
struct EnvTable {
    rows: Vec<EnvRow>,
    revision: u32,
    /// Why the stack file could not be read, so only the stored secrets are listed.
    error: Option<String>,
}

impl EnvTable {
    /// The rows of `status`'s active revision, service by service and key by key, then the
    /// stored secrets it does not reference. `secrets` are the stack's, as
    /// [`crate::orchestrator::Orchestrator::secrets`] lists them.
    fn new(status: &StackStatus, secrets: &[StackSecret], now: SystemTime) -> Self {
        let stack = &status.name;
        let prefix = format!("{stack}/");
        let known: BTreeMap<&str, &StackSecret> =
            secrets.iter().map(|s| (s.name.as_str(), s)).collect();
        let cell = |name: &str, referenced: bool| SecretCell {
            name: name.to_owned(),
            changed: known
                .get(name)
                .and_then(|s| s.updated_at)
                .map(|at| (Ago::since(at, now).to_string(), When(at).to_string())),
            referenced,
        };
        let mut rows = Vec::new();
        let mut referenced = Vec::new();
        let error = match status.revision.toml.parse::<Stack>() {
            Ok(definition) => {
                for (service, spec) in &definition.services {
                    let mut rows_of_service = Vec::new();
                    for (key, value) in spec.container_env() {
                        rows_of_service.push(EnvRow {
                            value: EnvValue::Var {
                                injected: !spec.env.contains_key(&key),
                                value,
                            },
                            key: Some(key),
                            service: Some(service.to_string()),
                        });
                    }
                    for (key, full) in &spec.secrets {
                        let name = full.strip_prefix(&prefix).unwrap_or(full);
                        referenced.push(name.to_owned());
                        rows_of_service.push(EnvRow {
                            key: Some(key.clone()),
                            service: Some(service.to_string()),
                            value: EnvValue::Secret {
                                full: full.clone(),
                                cell: cell(name, true),
                            },
                        });
                    }
                    rows_of_service.sort_by(|a, b| a.key.cmp(&b.key));
                    rows.extend(rows_of_service);
                }
                None
            }
            Err(err) => Some(err.to_string()),
        };
        for secret in secrets {
            if referenced.contains(&secret.name) {
                continue;
            }
            let services = (!secret.services.is_empty()).then(|| secret.services.join(", "));
            rows.push(EnvRow {
                key: None,
                service: services,
                value: EnvValue::Secret {
                    full: format!("{prefix}{}", secret.name),
                    cell: cell(&secret.name, !secret.services.is_empty()),
                },
            });
        }
        EnvTable {
            rows,
            revision: status.revision.revision,
            error,
        }
    }

    /// How many plain variables and distinct secrets the rows hold, for the card's head: a
    /// secret referenced by two services or under two keys counts once.
    fn counts(&self) -> (usize, usize) {
        let vars = self
            .rows
            .iter()
            .filter(|r| r.value.secret().is_none())
            .count();
        let secrets: BTreeSet<&str> = self
            .rows
            .iter()
            .filter_map(|r| r.value.secret())
            .map(|s| s.name.as_str())
            .collect();
        (vars, secrets.len())
    }
}

/// `GET /stacks/{name}/environment`.
#[derive(Template)]
#[template(path = "stack_environment.html")]
pub struct EnvironmentPage {
    layout: Layout,
    header: StackHeader,
    env: EnvTable,
    /// Where the set form posts; the delete forms post to `<action>/delete`.
    action: String,
    /// The stack's secret names, for the set form's datalist.
    names: Vec<String>,
    /// The name the set form starts with (`?set=`).
    prefill: String,
}

impl EnvironmentPage {
    fn url(stack: &str) -> String {
        format!("{}/environment", StackView::url(stack))
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
            .with_link(Self::url(stack), "Reload the environment")
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

/// The confirmation of deleting a referenced secret: the services it redeploys, and the name
/// to type.
#[derive(Template)]
#[template(path = "secret_delete_confirm.html")]
pub struct DeleteConfirm {
    action: String,
    secret: String,
    name: String,
    services: Vec<String>,
    /// Why the name typed was refused, shown above the field to type it again.
    error: Option<String>,
}

#[cfg(test)]
#[path = "environment_tests.rs"]
mod tests;
