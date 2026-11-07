//! The ACME account: created once per directory and persisted as `acme/account.json`, mode 0600
//! (ADR-0016, docs/architecture.md §7.3–7.4).
//!
//! The file is instant-acme's serialized [`AccountCredentials`], unchanged. Those record the
//! directory URL the account was registered at (their `directory` field). An account only exists
//! on the CA that issued it, so when the configured directory changes (say, from Let's Encrypt
//! staging to production) a new account is created and replaces the file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use instant_acme::{Account, AccountBuilder, AccountCredentials, NewAccount};
use serde::Deserialize;

use super::atomic::AtomicFile;
use crate::config::AcmeConfig;

/// Why the ACME account could not be loaded or created.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("cannot read ACME account file {}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("ACME account file {} is corrupt; move it away to register a new account", .path.display())]
    Corrupt {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot write ACME account file {}", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot set up the ACME HTTP client")]
    Client(#[source] instant_acme::Error),
    #[error("cannot register an ACME account at {directory}")]
    Create {
        directory: String,
        #[source]
        source: instant_acme::Error,
    },
    #[error("cannot restore the ACME account at {directory}")]
    Restore {
        directory: String,
        #[source]
        source: instant_acme::Error,
    },
}

/// The `directory` field of serialized [`AccountCredentials`]: the directory URL the account was
/// registered at, exactly as passed to `create`. instant-acme keeps the field crate-private but
/// documents it as part of the serialized form; credentials from old instant-acme versions lack it.
#[derive(Deserialize)]
struct CredentialsDirectory {
    directory: Option<String>,
}

/// A parsed `acme/account.json`. No `Debug`: it holds the account key.
struct StoredAccount {
    /// The directory URL the account was registered at, if the credentials record it.
    directory_url: Option<String>,
    credentials: AccountCredentials,
}

impl StoredAccount {
    /// The stored credentials if they belong to `directory_url`.
    fn into_credentials_for(self, directory_url: &str) -> Option<AccountCredentials> {
        (self.directory_url.as_deref() == Some(directory_url)).then_some(self.credentials)
    }
}

impl TryFrom<&[u8]> for StoredAccount {
    type Error = serde_json::Error;

    /// Parses the JSON of serialized [`AccountCredentials`].
    fn try_from(json: &[u8]) -> Result<Self, Self::Error> {
        let CredentialsDirectory { directory } = serde_json::from_slice(json)?;
        Ok(Self {
            directory_url: directory,
            credentials: serde_json::from_slice(json)?,
        })
    }
}

/// Loads, creates and persists hoster's ACME account under the state directory.
#[derive(Debug, Clone)]
pub struct AccountStore {
    path: PathBuf,
}

impl AccountStore {
    /// The account file's path relative to the state directory.
    const RELATIVE_PATH: &str = "acme/account.json";

    /// The store for the state directory `state_dir`.
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join(Self::RELATIVE_PATH),
        }
    }

    /// The account file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Restores the account registered at `config.directory_url`, or registers a new one (agreeing
    /// to the CA's terms of service) and persists its credentials.
    pub async fn load_or_create(&self, config: &AcmeConfig) -> Result<Account, AccountError> {
        let directory = config.directory_url.as_str();
        let stored = self.read()?;
        let previous = stored.as_ref().map(|s| {
            s.directory_url
                .clone()
                .unwrap_or_else(|| "unknown".to_owned())
        });
        if let Some(credentials) = stored.and_then(|s| s.into_credentials_for(directory)) {
            tracing::debug!(directory, "restoring ACME account");
            return Self::builder(config)?
                .from_credentials(credentials)
                .await
                .map_err(|source| AccountError::Restore {
                    directory: directory.to_owned(),
                    source,
                });
        }
        if let Some(previous) = previous {
            tracing::warn!(
                previous,
                directory,
                "ACME directory changed; registering a new account"
            );
        }

        let contact = config
            .contact
            .as_ref()
            .map(|email| format!("mailto:{email}"));
        let contact: Vec<&str> = contact.iter().map(String::as_str).collect();
        let new_account = NewAccount {
            contact: &contact,
            terms_of_service_agreed: true,
            only_return_existing: false,
        };
        let (account, credentials) = Self::builder(config)?
            .create(&new_account, directory.to_owned(), None)
            .await
            .map_err(|source| AccountError::Create {
                directory: directory.to_owned(),
                source,
            })?;
        self.write(&credentials)?;
        tracing::info!(directory, id = account.id(), "registered ACME account");
        Ok(account)
    }

    /// An instant-acme builder trusting the platform roots, or only `config.root_certificate`.
    fn builder(config: &AcmeConfig) -> Result<AccountBuilder, AccountError> {
        match &config.root_certificate {
            Some(root) => Account::builder_with_root(root),
            None => Account::builder(),
        }
        .map_err(AccountError::Client)
    }

    /// The stored account, or `None` when there is no account file yet.
    fn read(&self) -> Result<Option<StoredAccount>, AccountError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(AccountError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        StoredAccount::try_from(bytes.as_slice())
            .map(Some)
            .map_err(|source| AccountError::Corrupt {
                path: self.path.clone(),
                source,
            })
    }

    /// Writes `credentials` atomically (temporary file, fsync, rename) with mode 0600, creating
    /// the `acme` directory with mode 0700 if needed.
    fn write(&self, credentials: &AccountCredentials) -> Result<(), AccountError> {
        let error = |source| AccountError::Write {
            path: self.path.clone(),
            source,
        };
        let json = serde_json::to_vec_pretty(credentials)
            .map_err(io::Error::from)
            .map_err(error)?;
        AtomicFile::new(&self.path, AtomicFile::PRIVATE)
            .write(&json)
            .map_err(error)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";
    const PRODUCTION: &str = "https://acme-v02.api.letsencrypt.org/directory";

    fn credentials(directory: &str) -> AccountCredentials {
        serde_json::from_value(serde_json::json!({
            "id": "https://ca.example/acme/acct/1",
            "key_pkcs8": "AAECAw",
            "directory": directory,
        }))
        .expect("valid credentials JSON")
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn path_is_under_the_state_dir() {
        let store = AccountStore::new(Path::new("/srv/hoster"));
        assert_eq!(store.path(), Path::new("/srv/hoster/acme/account.json"));
    }

    #[test]
    fn missing_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        assert!(store.read().unwrap().is_none());
    }

    #[test]
    fn write_then_read_round_trips_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        store.write(&credentials(STAGING)).unwrap();

        assert_eq!(mode(store.path()), 0o600);
        assert_eq!(mode(store.path().parent().unwrap()), 0o700);
        assert!(!store.path().with_extension("json.tmp").exists());

        // The file is plain AccountCredentials (ADR-0016), not a hoster-specific wrapper.
        let file: AccountCredentials =
            serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        let original = serde_json::to_value(credentials(STAGING)).unwrap();
        assert_eq!(serde_json::to_value(&file).unwrap(), original);

        let read = store.read().unwrap().expect("account file exists");
        assert_eq!(read.directory_url.as_deref(), Some(STAGING));
        assert_eq!(serde_json::to_value(&read.credentials).unwrap(), original);
    }

    #[test]
    fn rewrite_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        store.write(&credentials(STAGING)).unwrap();
        store.write(&credentials(PRODUCTION)).unwrap();
        assert_eq!(
            store.read().unwrap().unwrap().directory_url.as_deref(),
            Some(PRODUCTION)
        );
        assert_eq!(mode(store.path()), 0o600);
    }

    #[test]
    fn credentials_are_bound_to_their_directory() {
        let stored = |directory: &str| {
            let json = serde_json::to_vec(&credentials(directory)).unwrap();
            StoredAccount::try_from(json.as_slice()).unwrap()
        };
        assert!(stored(STAGING).into_credentials_for(STAGING).is_some());
        assert!(stored(STAGING).into_credentials_for(PRODUCTION).is_none());
    }

    #[test]
    fn credentials_without_a_directory_match_none() {
        let json = serde_json::json!({
            "id": "https://ca.example/acme/acct/1",
            "key_pkcs8": "AAECAw",
            "urls": {
                "newNonce": "https://ca.example/nonce",
                "newAccount": "https://ca.example/account",
                "newOrder": "https://ca.example/order",
            },
        });
        let json = serde_json::to_vec(&json).unwrap();
        let stored = StoredAccount::try_from(json.as_slice()).unwrap();
        assert!(stored.directory_url.is_none());
        assert!(stored.into_credentials_for(STAGING).is_none());
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_new_account() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), b"{not json").unwrap();
        assert!(matches!(store.read(), Err(AccountError::Corrupt { .. })));
    }

    /// Against a Pebble container: creates an account, persists it, and restores it.
    ///
    /// ```sh
    /// podman run --rm -d -p 14000:14000 -e PEBBLE_VA_ALWAYS_VALID=1 ghcr.io/letsencrypt/pebble
    /// curl -fsSo /tmp/pebble.minica.pem \
    ///   https://raw.githubusercontent.com/letsencrypt/pebble/main/test/certs/pebble.minica.pem
    /// HOSTER_TEST_PEBBLE_DIRECTORY=https://localhost:14000/dir \
    /// HOSTER_TEST_PEBBLE_ROOT=/tmp/pebble.minica.pem \
    ///   cargo test pebble -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "needs a Pebble ACME server; see the doc comment"]
    async fn pebble_create_persist_restore() {
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let config = AcmeConfig {
            directory_url: env("HOSTER_TEST_PEBBLE_DIRECTORY"),
            contact: Some("admin@example.com".to_owned()),
            root_certificate: Some(PathBuf::from(env("HOSTER_TEST_PEBBLE_ROOT"))),
            ..AcmeConfig::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());

        let created = store.load_or_create(&config).await.expect("create account");
        assert_eq!(mode(store.path()), 0o600);
        let file = fs::read(store.path()).unwrap();

        let restored = store
            .load_or_create(&config)
            .await
            .expect("restore account");
        assert_eq!(restored.id(), created.id());
        assert_eq!(
            fs::read(store.path()).unwrap(),
            file,
            "restore must not rewrite"
        );

        // A different directory (same server, different URL) gets a new account file.
        let moved = AcmeConfig {
            directory_url: format!("{}?moved", config.directory_url),
            ..config.clone()
        };
        let other = store.load_or_create(&moved).await.expect("new account");
        assert_ne!(other.id(), created.id());
        assert_ne!(fs::read(store.path()).unwrap(), file);
        assert_eq!(
            store.read().unwrap().unwrap().directory_url,
            Some(moved.directory_url)
        );
    }
}
