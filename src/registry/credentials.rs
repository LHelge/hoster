//! Registry credentials keyed by registry host (ADR-0025).
//!
//! They live in the encrypted secret store (ADR-0029), entered under Settings → Registries:
//! [`StoredCredentials`] keeps one secret per host, named `_registry/<host>` (a `:` before a
//! port becomes `_`). Stack names start with a lowercase letter, so that namespace can never
//! collide with an app secret (`<stack>/<name>`). Callers only see the [`RegistryCredentials`]
//! trait.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;

use crate::secrets::{SecretError, SecretName, SecretStore, SecretValue};
use crate::store::Timestamp;

/// A username and secret (for ghcr: any username and a classic PAT with `read:packages`).
///
/// `Debug` never prints the secret, so a credential can sit in logged structs safely.
#[derive(Clone, PartialEq, Eq)]
pub struct RegistryCredential {
    username: String,
    secret: String,
}

impl RegistryCredential {
    pub fn new(username: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            secret: secret.into(),
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// The password or token. Never log it.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// The stored form, `username:secret`, as a secret value.
    fn sealable(&self) -> SecretValue {
        SecretValue::from(format!("{}:{}", self.username, self.secret))
    }
}

impl fmt::Debug for RegistryCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegistryCredential")
            .field("username", &self.username)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Parses `username:secret` (one line; a trailing newline is ignored). The secret may itself
/// contain `:`. The error never echoes the input, which holds the secret.
impl FromStr for RegistryCredential {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let line = s.trim_end_matches(['\n', '\r']);
        match line.split_once(':') {
            Some((user, secret))
                if !user.is_empty() && !secret.is_empty() && !line.contains('\n') =>
            {
                Ok(Self::new(user, secret))
            }
            _ => Err("expected a single line `username:token`".to_owned()),
        }
    }
}

/// A registry host as images name it: `ghcr.io`, `docker.io`, `localhost:5000`. Lowercase
/// letters, digits, `.` and `-`, with an optional `:<port>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegistryHost(String);

impl RegistryHost {
    /// The secret-store namespace of registry credentials.
    const NAMESPACE: &str = "_registry/";

    /// The name of this host's credential in the secret store.
    fn secret_name(&self) -> SecretName {
        format!("{}{}", Self::NAMESPACE, self.0.replace(':', "_"))
            .parse()
            .expect("a valid host is a valid secret name")
    }

    /// The host a secret-store name belongs to, if it is a registry credential.
    fn from_secret_name(name: &str) -> Option<Self> {
        name.strip_prefix(Self::NAMESPACE)?
            .replace('_', ":")
            .parse()
            .ok()
    }
}

impl FromStr for RegistryHost {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        let (name, port) = match s.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (s.as_str(), None),
        };
        let name_ok = !name.is_empty()
            && name.len() <= 253
            && name.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            });
        let port_ok = port.is_none_or(|p| p.parse::<u16>().is_ok_and(|p| p > 0));
        if name_ok && port_ok {
            Ok(Self(s))
        } else {
            Err(format!(
                "`{s}` is not a registry host such as ghcr.io, docker.io or localhost:5000"
            ))
        }
    }
}

impl AsRef<str> for RegistryHost {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RegistryHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a credential could not be looked up or stored. Messages name the registry, never the
/// secret.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("invalid registry host `{registry}`: {reason}")]
    InvalidHost { registry: String, reason: String },
    #[error("invalid credential for {registry}: {reason}")]
    Invalid { registry: String, reason: String },
    #[error("the stored credential for {registry} is malformed: {reason}")]
    Malformed { registry: String, reason: String },
    #[error("cannot access the credential for {registry} in the secret store: {source}")]
    Store {
        registry: String,
        #[source]
        source: SecretError,
    },
}

impl CredentialError {
    /// The registry host the lookup was for.
    pub fn registry(&self) -> &str {
        match self {
            Self::InvalidHost { registry, .. }
            | Self::Invalid { registry, .. }
            | Self::Malformed { registry, .. }
            | Self::Store { registry, .. } => registry,
        }
    }
}

/// Looks up the credential for a registry host (`ghcr.io`, `docker.io`, `localhost:5000`, …).
///
/// `Ok(None)` means "pull anonymously". The future is boxed so the provider can sit behind
/// `Arc<dyn RegistryCredentials>`.
pub trait RegistryCredentials: fmt::Debug + Send + Sync {
    fn lookup<'a>(
        &'a self,
        registry: &'a str,
    ) -> BoxFuture<'a, Result<Option<RegistryCredential>, CredentialError>>;
}

/// No credentials for any registry: every pull is anonymous.
#[derive(Debug, Clone, Copy, Default)]
pub struct Anonymous;

impl RegistryCredentials for Anonymous {
    fn lookup<'a>(
        &'a self,
        _registry: &'a str,
    ) -> BoxFuture<'a, Result<Option<RegistryCredential>, CredentialError>> {
        futures_util::future::ready(Ok(None)).boxed()
    }
}

/// A fixed set of credentials, keyed by registry host (tests, or a provider built elsewhere).
impl RegistryCredentials for BTreeMap<String, RegistryCredential> {
    fn lookup<'a>(
        &'a self,
        registry: &'a str,
    ) -> BoxFuture<'a, Result<Option<RegistryCredential>, CredentialError>> {
        futures_util::future::ready(Ok(self.get(registry).cloned())).boxed()
    }
}

/// A stored registry credential as Settings → Registries lists it: the token is never listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryEntry {
    pub host: RegistryHost,
    pub username: String,
    pub updated_at: Timestamp,
}

/// The registry credentials in the secret store (ADR-0025, ADR-0029). Cheap to clone.
#[derive(Debug, Clone)]
pub struct StoredCredentials {
    secrets: SecretStore,
}

impl From<SecretStore> for StoredCredentials {
    fn from(secrets: SecretStore) -> Self {
        Self { secrets }
    }
}

impl StoredCredentials {
    /// Stores (or replaces) the credential for `host`. The username must not contain `:`, and
    /// neither part may be empty or span lines.
    pub async fn set(
        &self,
        host: &RegistryHost,
        credential: &RegistryCredential,
    ) -> Result<(), CredentialError> {
        let invalid = |reason: &str| CredentialError::Invalid {
            registry: host.to_string(),
            reason: reason.to_owned(),
        };
        if credential.username().contains(':') {
            return Err(invalid("the username must not contain `:`"));
        }
        let sealable = credential.sealable();
        let parsed: RegistryCredential = sealable
            .expose()
            .parse()
            .map_err(|_| invalid("enter a username and a token, each on one line"))?;
        if parsed != *credential {
            return Err(invalid("enter a username and a token, each on one line"));
        }
        self.secrets
            .set(&host.secret_name(), &sealable)
            .await
            .map_err(|source| CredentialError::Store {
                registry: host.to_string(),
                source,
            })?;
        tracing::info!(registry = %host, "registry credential stored");
        Ok(())
    }

    /// Removes the credential for `host`. Returns whether there was one.
    pub async fn remove(&self, host: &RegistryHost) -> Result<bool, CredentialError> {
        self.secrets
            .remove(&host.secret_name())
            .await
            .map_err(|source| CredentialError::Store {
                registry: host.to_string(),
                source,
            })
    }

    /// Every stored credential's host, username and last change, by host.
    pub async fn list(&self) -> Result<Vec<RegistryEntry>, CredentialError> {
        let store_error = |source| CredentialError::Store {
            registry: "any registry".to_owned(),
            source,
        };
        let mut out = Vec::new();
        for entry in self.secrets.list().await.map_err(store_error)? {
            let Some(host) = RegistryHost::from_secret_name(&entry.name) else {
                continue;
            };
            let username = match self.read(&host).await {
                Ok(Some(credential)) => credential.username,
                Ok(None) => continue,
                Err(err) => {
                    tracing::warn!(registry = %host, error = %err, "unreadable registry credential");
                    String::new()
                }
            };
            out.push(RegistryEntry {
                host,
                username,
                updated_at: entry.updated_at,
            });
        }
        out.sort_by(|a, b| a.host.cmp(&b.host));
        Ok(out)
    }

    async fn read(
        &self,
        host: &RegistryHost,
    ) -> Result<Option<RegistryCredential>, CredentialError> {
        let Some((value, _)) = self
            .secrets
            .reveal(host.secret_name().as_ref())
            .await
            .map_err(|source| CredentialError::Store {
                registry: host.to_string(),
                source,
            })?
        else {
            return Ok(None);
        };
        value
            .expose()
            .parse()
            .map(Some)
            .map_err(|reason| CredentialError::Malformed {
                registry: host.to_string(),
                reason,
            })
    }
}

impl RegistryCredentials for StoredCredentials {
    fn lookup<'a>(
        &'a self,
        registry: &'a str,
    ) -> BoxFuture<'a, Result<Option<RegistryCredential>, CredentialError>> {
        async move {
            let host: RegistryHost =
                registry
                    .parse()
                    .map_err(|reason| CredentialError::InvalidHost {
                        registry: registry.to_owned(),
                        reason,
                    })?;
            self.read(&host).await
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::secrets::SecretKey;
    use crate::store::Store;

    const TOKEN: &str = "ghp_supersecrettoken";

    async fn stored(dir: &TempDir) -> (StoredCredentials, SecretStore) {
        let store = Store::open(&dir.path().join("state")).await.unwrap();
        let secrets = SecretStore::new(store, &SecretKey::generate());
        (StoredCredentials::from(secrets.clone()), secrets)
    }

    fn host(s: &str) -> RegistryHost {
        s.parse().unwrap()
    }

    #[test]
    fn credential_parses_and_redacts() {
        let credential: RegistryCredential = format!("bot:{TOKEN}\n").parse().unwrap();
        assert_eq!(credential.username(), "bot");
        assert_eq!(credential.secret(), TOKEN);
        assert!(!format!("{credential:?}").contains(TOKEN));
        let colon: RegistryCredential = "bot:a:b".parse().unwrap();
        assert_eq!(colon.secret(), "a:b");
        // The error is fixed text: it never echoes the input, which may be the secret.
        for bad in ["", "bot", ":token", "bot:", "bot:a\nother:b", TOKEN] {
            let err = bad.parse::<RegistryCredential>().unwrap_err();
            assert_eq!(err, "expected a single line `username:token`", "{bad:?}");
        }
    }

    #[test]
    fn registry_hosts() {
        for good in [
            "ghcr.io",
            "docker.io",
            "localhost:5000",
            "registry-1.example.com",
        ] {
            assert_eq!(host(good).to_string(), good);
            assert_eq!(
                RegistryHost::from_secret_name(host(good).secret_name().as_ref()),
                Some(host(good))
            );
        }
        assert_eq!(host(" GHCR.io ").as_ref(), "ghcr.io");
        assert_eq!(
            host("localhost:5000").secret_name().as_ref(),
            "_registry/localhost_5000"
        );
        for bad in [
            "",
            "../etc",
            ".hidden",
            "a/b",
            "-a.io",
            "a..io",
            "host:0",
            "host:http",
            "a_b",
        ] {
            assert!(bad.parse::<RegistryHost>().is_err(), "{bad}");
        }
        // App secrets are not registry credentials.
        assert_eq!(RegistryHost::from_secret_name("notes/db"), None);
    }

    #[tokio::test]
    async fn stored_credentials_round_trip_by_host() {
        let dir = TempDir::new().unwrap();
        let (credentials, secrets) = stored(&dir).await;
        assert!(credentials.lookup("ghcr.io").await.unwrap().is_none());

        credentials
            .set(&host("ghcr.io"), &RegistryCredential::new("bot", TOKEN))
            .await
            .unwrap();
        credentials
            .set(
                &host("localhost:5000"),
                &RegistryCredential::new("dev", "pw"),
            )
            .await
            .unwrap();
        // An app secret is not listed.
        secrets
            .set(&"notes/db".parse().unwrap(), &SecretValue::from("x"))
            .await
            .unwrap();

        let ghcr = credentials.lookup("ghcr.io").await.unwrap().unwrap();
        assert_eq!((ghcr.username(), ghcr.secret()), ("bot", TOKEN));
        let local = credentials.lookup("localhost:5000").await.unwrap().unwrap();
        assert_eq!(local.username(), "dev");
        assert!(credentials.lookup("docker.io").await.unwrap().is_none());

        let listed = credentials.list().await.unwrap();
        let hosts: Vec<_> = listed.iter().map(|e| e.host.to_string()).collect();
        assert_eq!(hosts, ["ghcr.io", "localhost:5000"]);
        assert_eq!(listed[0].username, "bot");
        assert!(!format!("{listed:?}").contains(TOKEN));

        // Replacing keeps one entry; removing drops it.
        credentials
            .set(&host("ghcr.io"), &RegistryCredential::new("bot2", "new"))
            .await
            .unwrap();
        assert_eq!(
            credentials
                .lookup("ghcr.io")
                .await
                .unwrap()
                .unwrap()
                .secret(),
            "new"
        );
        assert!(credentials.remove(&host("ghcr.io")).await.unwrap());
        assert!(!credentials.remove(&host("ghcr.io")).await.unwrap());
        assert!(credentials.lookup("ghcr.io").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn invalid_credentials_and_hosts_are_refused_without_echoing_secrets() {
        let dir = TempDir::new().unwrap();
        let (credentials, _) = stored(&dir).await;
        for bad in [
            RegistryCredential::new("a:b", TOKEN),
            RegistryCredential::new("", TOKEN),
            RegistryCredential::new("bot", ""),
            RegistryCredential::new("bot", format!("{TOKEN}\nmore")),
        ] {
            let err = credentials.set(&host("ghcr.io"), &bad).await.unwrap_err();
            assert!(matches!(err, CredentialError::Invalid { .. }), "{err:?}");
            assert!(!err.to_string().contains(TOKEN), "{err}");
        }
        let err = credentials.lookup("../etc").await.unwrap_err();
        assert!(
            matches!(err, CredentialError::InvalidHost { .. }),
            "{err:?}"
        );
        assert_eq!(err.registry(), "../etc");
    }

    #[tokio::test]
    async fn anonymous_and_static_providers() {
        assert!(Anonymous.lookup("ghcr.io").await.unwrap().is_none());
        let map = BTreeMap::from([("ghcr.io".to_owned(), RegistryCredential::new("u", "p"))]);
        assert_eq!(
            map.lookup("ghcr.io").await.unwrap(),
            Some(RegistryCredential::new("u", "p"))
        );
        assert!(map.lookup("docker.io").await.unwrap().is_none());
    }
}
