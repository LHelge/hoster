//! Registry credentials keyed by registry host (ADR-0025).
//!
//! The long-term home is the encrypted secret store (ADR-0029), edited under Settings →
//! Registries. Until it exists, [`FileCredentials`] reads one 0600 file per host from
//! `<state_dir>/registries/`. Callers only see the [`RegistryCredentials`] trait, so the switch
//! does not touch them.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use futures_util::FutureExt;
use futures_util::future::BoxFuture;
use tokio::io::AsyncReadExt;

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

/// Why a credential could not be looked up. Messages name the registry and file, never the
/// secret.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("invalid registry host `{registry}` for a credential lookup")]
    InvalidHost { registry: String },
    #[error("cannot read the credential for {registry} from {}: {source}", path.display())]
    Read {
        registry: String,
        path: PathBuf,
        source: io::Error,
    },
    #[error(
        "the credential file for {registry}, {}, is readable by others (mode {mode:o}); \
         run `chmod 600` on it", path.display()
    )]
    Permissions {
        registry: String,
        path: PathBuf,
        mode: u32,
    },
    #[error("malformed credential file for {registry}, {}: {reason}", path.display())]
    Malformed {
        registry: String,
        path: PathBuf,
        reason: String,
    },
}

impl CredentialError {
    /// The registry host the lookup was for.
    pub fn registry(&self) -> &str {
        match self {
            Self::InvalidHost { registry }
            | Self::Read { registry, .. }
            | Self::Permissions { registry, .. }
            | Self::Malformed { registry, .. } => registry,
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

/// One file per registry host, `<dir>/<host>`, holding `username:token` and readable by its
/// owner only (mode 0600 or stricter). A missing file means anonymous pulls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCredentials {
    dir: PathBuf,
}

impl FileCredentials {
    /// The directory under hoster's state directory that holds the files.
    pub const DIR_NAME: &str = "registries";

    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// `<state_dir>/registries`.
    pub fn in_state_dir(state_dir: &Path) -> Self {
        Self::new(state_dir.join(Self::DIR_NAME))
    }

    #[allow(
        dead_code,
        reason = "for messages that point the admin at the directory"
    )]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file for `registry`, refusing hosts that would escape the directory.
    fn path(&self, registry: &str) -> Result<PathBuf, CredentialError> {
        let valid = !registry.is_empty()
            && !registry.starts_with('.')
            && registry
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c));
        if !valid {
            return Err(CredentialError::InvalidHost {
                registry: registry.to_owned(),
            });
        }
        Ok(self.dir.join(registry))
    }

    async fn read(&self, registry: &str) -> Result<Option<RegistryCredential>, CredentialError> {
        let path = self.path(registry)?;
        let read_error = |path: &Path, source| CredentialError::Read {
            registry: registry.to_owned(),
            path: path.to_path_buf(),
            source,
        };
        // Open once and check the mode of the opened file, so the file that is checked is the
        // file that is read even if the path is swapped in between.
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(read_error(&path, e)),
        };
        let metadata = file.metadata().await.map_err(|e| read_error(&path, e))?;
        if !metadata.is_file() {
            return Err(read_error(
                &path,
                io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
            ));
        }
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(CredentialError::Permissions {
                registry: registry.to_owned(),
                path,
                mode,
            });
        }
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .await
            .map_err(|e| read_error(&path, e))?;
        contents
            .parse()
            .map(Some)
            .map_err(|reason| CredentialError::Malformed {
                registry: registry.to_owned(),
                path,
                reason,
            })
    }
}

impl RegistryCredentials for FileCredentials {
    fn lookup<'a>(
        &'a self,
        registry: &'a str,
    ) -> BoxFuture<'a, Result<Option<RegistryCredential>, CredentialError>> {
        self.read(registry).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "ghp_supersecrettoken";

    fn write(dir: &Path, name: &str, contents: &str, mode: u32) {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
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

    #[tokio::test]
    async fn file_credentials_are_keyed_by_host() {
        let state = tempfile::tempdir().unwrap();
        let provider = FileCredentials::in_state_dir(state.path());
        assert_eq!(provider.dir(), state.path().join("registries"));
        // No directory yet: anonymous.
        assert!(provider.lookup("ghcr.io").await.unwrap().is_none());

        std::fs::create_dir(provider.dir()).unwrap();
        write(provider.dir(), "ghcr.io", &format!("bot:{TOKEN}\n"), 0o600);
        write(provider.dir(), "localhost:5000", "dev:pw", 0o400);
        let ghcr = provider.lookup("ghcr.io").await.unwrap().unwrap();
        assert_eq!((ghcr.username(), ghcr.secret()), ("bot", TOKEN));
        let local = provider.lookup("localhost:5000").await.unwrap().unwrap();
        assert_eq!(local.username(), "dev");
        assert!(provider.lookup("docker.io").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn file_credentials_must_be_private() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ghcr.io", &format!("bot:{TOKEN}"), 0o644);
        let err = FileCredentials::new(dir.path())
            .lookup("ghcr.io")
            .await
            .unwrap_err();
        assert!(
            matches!(err, CredentialError::Permissions { mode: 0o644, .. }),
            "{err:?}"
        );
        assert_eq!(err.registry(), "ghcr.io");
        assert!(err.to_string().contains("chmod 600"));
        assert!(!err.to_string().contains(TOKEN));

        std::fs::create_dir(dir.path().join("docker.io")).unwrap();
        let err = FileCredentials::new(dir.path())
            .lookup("docker.io")
            .await
            .unwrap_err();
        assert!(matches!(err, CredentialError::Read { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn malformed_files_do_not_leak_their_contents() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ghcr.io", TOKEN, 0o600);
        let err = FileCredentials::new(dir.path())
            .lookup("ghcr.io")
            .await
            .unwrap_err();
        assert!(matches!(err, CredentialError::Malformed { .. }), "{err:?}");
        assert!(!err.to_string().contains(TOKEN), "{err}");
        assert!(!format!("{err:?}").contains(TOKEN));
    }

    #[tokio::test]
    async fn hosts_cannot_escape_the_directory() {
        let provider = FileCredentials::new("/nonexistent");
        for host in ["", "../etc/passwd", ".hidden", "a/b"] {
            assert!(
                matches!(
                    provider.lookup(host).await,
                    Err(CredentialError::InvalidHost { .. })
                ),
                "{host}"
            );
        }
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
