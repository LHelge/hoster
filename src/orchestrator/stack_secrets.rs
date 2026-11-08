//! A stack's secrets (ADR-0029) and the redeploy a change to one starts (ADR-0049).
//!
//! The UI lists a stack's secret names with their last change and the services of the active
//! revision that reference each one, never the values. Setting, replacing or deleting a secret
//! redeploys exactly the services whose config hash moves with it, in one `source = apply`
//! deployment. A secret no service references is stored or deleted without a deployment.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::engine::Target;
use super::{DeployOutcome, Orchestrator, OrchestratorError, Result};
use crate::runtime::ContainerRuntime;
use crate::secrets::{SecretError, SecretName, SecretValue};
use crate::stack::Stack;
use crate::store::{DeploymentSource, DigestSet, StackRevision, Timestamp};

/// One secret of a stack, as its Secrets page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackSecret {
    /// The name within the stack: `db` for the secret `notes/db`.
    pub name: String,
    /// When it was last set; `None` when the active revision references it but it is not set.
    pub updated_at: Option<Timestamp>,
    /// The services of the active revision that reference it.
    pub services: Vec<String>,
}

/// The reply to [`Orchestrator::set_secret`] and [`Orchestrator::delete_secret`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretChange {
    /// The full secret name, `<stack>/<name>`.
    pub secret: String,
    /// Whether the secret existed before.
    pub existed: bool,
    /// The services that reference it, which the change redeploys.
    pub services: Vec<String>,
    /// The `source = apply` deployment; `None` when no service references the secret.
    pub deployment: Option<DeployOutcome>,
}

impl<R: ContainerRuntime> Orchestrator<R> {
    /// Every secret of `stack`: the ones stored under `<stack>/`, and the ones its active
    /// revision references but that are not set, by name. Values are never read.
    pub async fn secrets(&self, stack: &str) -> Result<Vec<StackSecret>> {
        let definition = self.active_definition(stack).await?;
        let users = Self::secret_users(&definition);
        let prefix = format!("{stack}/");
        let mut secrets: BTreeMap<String, StackSecret> = BTreeMap::new();
        for entry in self.secrets.list().await? {
            if let Some(name) = entry.name.strip_prefix(&prefix) {
                secrets.insert(
                    name.to_owned(),
                    StackSecret {
                        name: name.to_owned(),
                        updated_at: Some(entry.updated_at),
                        services: users.get(&entry.name).cloned().unwrap_or_default(),
                    },
                );
            }
        }
        for (full, services) in users {
            if let Some(name) = full.strip_prefix(&prefix) {
                secrets
                    .entry(name.to_owned())
                    .or_insert_with(|| StackSecret {
                        name: name.to_owned(),
                        updated_at: None,
                        services,
                    });
            }
        }
        Ok(secrets.into_values().collect())
    }

    /// Sets (or replaces) the secret `name` of `stack` and redeploys the services that
    /// reference it (ADR-0049). The value is never logged.
    #[tracing::instrument(skip(self, value))]
    pub async fn set_secret(
        self: &Arc<Self>,
        stack: &str,
        name: &str,
        value: &SecretValue,
    ) -> Result<SecretChange> {
        let secret = Self::secret_name(stack, name)?;
        let revision = self.active_revision(stack).await?;
        let definition: Stack = revision.toml.parse()?;
        let existed = self.secrets.version(secret.as_ref()).await?.is_some();
        self.secrets.set(&secret, value).await?;
        self.redeploy_for(revision, definition, secret, existed)
            .await
    }

    /// Deletes the secret `name` of `stack` and redeploys the services that reference it
    /// (ADR-0049). Those deploys fail until the secret is set again or the stack stops
    /// referencing it; the running containers keep serving meanwhile. The UI asks for a typed
    /// confirmation first.
    #[tracing::instrument(skip(self))]
    pub async fn delete_secret(self: &Arc<Self>, stack: &str, name: &str) -> Result<SecretChange> {
        let secret = Self::secret_name(stack, name)?;
        let revision = self.active_revision(stack).await?;
        let definition: Stack = revision.toml.parse()?;
        let existed = self.secrets.remove(&secret).await?;
        if !existed {
            return Ok(SecretChange {
                secret: secret.to_string(),
                existed,
                services: Vec::new(),
                deployment: None,
            });
        }
        self.redeploy_for(revision, definition, secret, existed)
            .await
    }

    /// Redeploys `revision` (the stack's active one, parsed as `definition`) for a change of
    /// `secret`, unless no service references it. The services listed and the deployment's
    /// target come from the same revision, so an apply landing meanwhile cannot pair one
    /// revision's stack file with another revision's record.
    async fn redeploy_for(
        self: &Arc<Self>,
        revision: StackRevision,
        definition: Stack,
        secret: SecretName,
        existed: bool,
    ) -> Result<SecretChange> {
        let services = Self::secret_users(&definition)
            .remove(secret.as_ref())
            .unwrap_or_default();
        let deployment = if services.is_empty() {
            None
        } else {
            // The revision again: only the services whose config hash moved with the secret's
            // version change.
            let pins = self.store.revision_images(revision.id).await?;
            let target = Target::new(Arc::new(definition), revision, pins, DigestSet::new());
            Some(self.submit(target, DeploymentSource::Apply).await?)
        };
        Ok(SecretChange {
            secret: secret.to_string(),
            existed,
            services,
            deployment,
        })
    }

    /// The parsed active revision of `stack`, which must exist and not be removed.
    async fn active_definition(&self, stack: &str) -> Result<Stack> {
        Ok(self.active_revision(stack).await?.toml.parse()?)
    }

    /// `<stack>/<name>`, where `name` is one segment as stack files may reference it.
    fn secret_name(stack: &str, name: &str) -> Result<SecretName> {
        let full = format!("{stack}/{}", name.trim());
        if name.trim().contains('/') {
            return Err(OrchestratorError::Secret(SecretError::InvalidName {
                name: full,
                reason: "the name within the stack must not contain `/`".to_owned(),
            }));
        }
        Ok(full.parse()?)
    }

    /// Secret name → the services that reference it.
    fn secret_users(definition: &Stack) -> BTreeMap<String, Vec<String>> {
        let mut users: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (service, spec) in &definition.services {
            for secret in spec.secrets.values() {
                let list = users.entry(secret.clone()).or_default();
                if !list.iter().any(|s| s == service.as_ref()) {
                    list.push(service.to_string());
                }
            }
        }
        users
    }
}
