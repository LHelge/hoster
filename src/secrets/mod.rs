//! The encrypted secret store (ADR-0029): app secrets (`<stack>/<name>`, injected into containers
//! as environment variables) and hoster's own registry credentials (`_registry/<host>`,
//! ADR-0025).
//!
//! Values are sealed with XChaCha20-Poly1305 under a [`SecretKey`] that lives outside the state
//! directory: the user-scoped systemd credential `secret_key` if there is one, else a 0600 key
//! file (by default `~/.config/hoster/secret.key`) that hoster generates on first start. Every
//! write draws a fresh 24-byte nonce, and the secret's name is the associated data, so a sealed
//! value cannot be moved to another name. The nonce doubles as the secret's
//! [`SecretVersion`], which joins the per-service config hash.
//!
//! [`SecretStore`] is write-only towards the UI: set, replace, remove and list names. Only the
//! deploy engine (at container create) and the registry credentials (for pulls and token
//! requests) call [`SecretStore::reveal`]. [`SecretValue`] never prints its contents, so values
//! cannot slip into logs through `Debug`.

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use thiserror::Error;

use crate::config::Credentials;
use crate::store::{Event, NewEvent, SealedSecret, SecretEntry, Store, StoreError, Timestamp};

/// The systemd credential that supplies the key instead of the key file (ADR-0029).
pub const KEY_CREDENTIAL: &str = "secret_key";

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const MAX_NAME_LEN: usize = 255;

/// Why a secret operation failed. Messages name secrets and files, never values or key material.
#[derive(Debug, Error)]
pub enum SecretError {
    #[error("invalid secret name `{name}`: {reason}")]
    InvalidName { name: String, reason: String },
    #[error("cannot read the secret-store key from {}", path.display())]
    ReadKey {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot create the secret-store key file {}", path.display())]
    WriteKey {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "the secret-store key file {} has mode {mode:o}, which lets other users read it; chmod 600 it",
        path.display()
    )]
    KeyPermissions { path: PathBuf, mode: u32 },
    #[error(
        "the secret-store key file {} does not exist, but {stored} secret(s) are stored under a \
         key: restore the key file (or the `secret_key` credential) from its backup",
        path.display()
    )]
    KeyMissing { path: PathBuf, stored: usize },
    #[error("the secret-store key from {source_name} is invalid: expected 32 base64-encoded bytes")]
    InvalidKey { source_name: String },
    #[error(transparent)]
    Credential(#[from] crate::config::ConfigError),
    #[error(
        "cannot decrypt secret `{0}`: it was sealed under another key, or its row was tampered with"
    )]
    Decrypt(String),
    #[error("cannot encrypt secret `{0}`")]
    Encrypt(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

type Result<T, E = SecretError> = std::result::Result<T, E>;

/// A secret's name: one or more `/`-separated segments of `[A-Za-z0-9._-]`, none of them `.` or
/// `..`, at most 255 bytes. App secrets are `<stack>/<name>` (ADR-0029).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretName(String);

impl FromStr for SecretName {
    type Err = SecretError;

    fn from_str(s: &str) -> Result<Self> {
        let invalid = |reason: &str| SecretError::InvalidName {
            name: s.to_owned(),
            reason: reason.to_owned(),
        };
        if s.is_empty() || s.len() > MAX_NAME_LEN {
            return Err(invalid("must be 1 to 255 bytes long"));
        }
        for segment in s.split('/') {
            if segment.is_empty() || segment == "." || segment == ".." {
                return Err(invalid(
                    "segments between `/` must not be empty, `.` or `..`",
                ));
            }
            if !segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            {
                return Err(invalid(
                    "only A-Z, a-z, 0-9, `.`, `_`, `-` and `/` are allowed",
                ));
            }
        }
        Ok(Self(s.to_owned()))
    }
}

impl AsRef<str> for SecretName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A secret's plaintext. `Debug` and `Display` never show it; [`SecretValue::expose`] is the only
/// way in, and its callers hand the value straight to podman.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretValue(String);

impl SecretValue {
    /// The plaintext. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretValue {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SecretValue {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Form fields deserialize straight into a value, so the plaintext never sits in a `String`
/// that something could print.
impl<'de> serde::Deserialize<'de> for SecretValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

impl SecretValue {
    /// Whether the value is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The value with browser line endings (`\r\n`) made `\n`.
    pub fn normalized(self) -> Self {
        if self.0.contains('\r') {
            Self(self.0.replace("\r\n", "\n"))
        } else {
            self
        }
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Which write of a secret is stored: the hex of its nonce, which is fresh for every write.
/// Joins the per-service config hash, so replacing a secret redeploys its services (ADR-0029).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretVersion(String);

impl From<&SealedSecret> for SecretVersion {
    fn from(secret: &SealedSecret) -> Self {
        Self(secret.nonce.iter().map(|b| format!("{b:02x}")).collect())
    }
}

impl AsRef<str> for SecretVersion {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The 256-bit key secrets are sealed under. Its text form (key file, systemd credential) is one
/// line of standard base64. `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretKey([u8; KEY_LEN]);

impl SecretKey {
    /// A fresh random key.
    pub fn generate() -> Self {
        let mut bytes = [0; KEY_LEN];
        getrandom::fill(&mut bytes).expect("the OS random number generator works");
        Self(bytes)
    }

    /// The key from the systemd credential [`KEY_CREDENTIAL`] if one is loaded, else from
    /// `file`. `None` when neither exists yet. An existing file must not be readable by group or
    /// others.
    pub fn load(file: &Path, credentials: &Credentials) -> Result<Option<Self>> {
        if let Some(text) = credentials.read(KEY_CREDENTIAL)? {
            return Self::decode(&text, &format!("the systemd credential {KEY_CREDENTIAL}"))
                .map(Some);
        }
        match fs::metadata(file) {
            Ok(meta) => {
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    return Err(SecretError::KeyPermissions {
                        path: file.to_owned(),
                        mode,
                    });
                }
                let text = fs::read_to_string(file).map_err(|source| SecretError::ReadKey {
                    path: file.to_owned(),
                    source,
                })?;
                Self::decode(&text, &file.display().to_string()).map(Some)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SecretError::ReadKey {
                path: file.to_owned(),
                source,
            }),
        }
    }

    /// Generates a fresh key into the new file `file` (mode 0600, parent directory 0700). Never
    /// replaces an existing file.
    pub fn generate_into(file: &Path) -> Result<Self> {
        let key = Self::generate();
        key.create_file(file)
            .map_err(|source| SecretError::WriteKey {
                path: file.to_owned(),
                source,
            })?;
        tracing::info!(path = %file.display(), "generated the secret-store key; back it up");
        Ok(key)
    }

    fn decode(text: &str, source_name: &str) -> Result<Self> {
        let invalid = || SecretError::InvalidKey {
            source_name: source_name.to_owned(),
        };
        let bytes = BASE64
            .decode(text.trim_end_matches(['\n', '\r']))
            .map_err(|_| invalid())?;
        Ok(Self(bytes.try_into().map_err(|_| invalid())?))
    }

    /// Writes the key to the new file `path`, never replacing an existing one: it is staged in
    /// `<path>.tmp` (0600), fsynced and hard-linked into place, so a crash leaves no torn key.
    fn create_file(&self, path: &Path) -> io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("path has no parent directory"))?;
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let _ = fs::remove_file(&tmp);
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            writeln!(file, "{}", BASE64.encode(self.0))?;
            file.sync_all()?;
            fs::hard_link(&tmp, path)?;
            fs::File::open(dir)?.sync_all()
        })();
        let _ = fs::remove_file(&tmp);
        result
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

/// The secret store: sealed rows in the state store, opened with the key. Cheap to clone.
#[derive(Clone)]
pub struct SecretStore {
    store: Store,
    cipher: Arc<XChaCha20Poly1305>,
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretStore").finish_non_exhaustive()
    }
}

impl SecretStore {
    /// Opens the store with the key from [`SecretKey::load`]. On the first start (no key yet and
    /// no stored secrets) the key file is generated. A missing key while secrets are stored is an
    /// error rather than a new key: those secrets could never be opened again, and the operator
    /// should restore the key from the backup instead (ADR-0029).
    pub async fn open(store: Store, key_file: &Path, credentials: &Credentials) -> Result<Self> {
        let key = match SecretKey::load(key_file, credentials)? {
            Some(key) => key,
            None => {
                let stored = store.secret_entries().await?.len();
                if stored > 0 {
                    return Err(SecretError::KeyMissing {
                        path: key_file.to_owned(),
                        stored,
                    });
                }
                SecretKey::generate_into(key_file)?
            }
        };
        Ok(Self::new(store, &key))
    }

    pub fn new(store: Store, key: &SecretKey) -> Self {
        Self {
            store,
            cipher: Arc::new(XChaCha20Poly1305::new(&key.0.into())),
        }
    }

    /// Sets (or replaces) the secret `name` without recording it. Returns its new version.
    /// Only tests write unrecorded: every production write goes through
    /// [`set_recorded`](Self::set_recorded) (ADR-0055).
    #[cfg(test)]
    pub async fn set(&self, name: &SecretName, value: &SecretValue) -> Result<SecretVersion> {
        let sealed = self.seal(name, value)?;
        self.store.put_secret(&sealed).await?;
        tracing::info!(secret = %name, "secret set");
        Ok(SecretVersion::from(&sealed))
    }

    /// Sets (or replaces) the secret `name`, recording `event` in the event log in the same
    /// transaction (ADR-0055). The event must name the secret, never its value. Returns the new
    /// version and the stored event.
    pub async fn set_recorded(
        &self,
        name: &SecretName,
        value: &SecretValue,
        event: &NewEvent,
    ) -> Result<(SecretVersion, Event)> {
        let sealed = self.seal(name, value)?;
        let recorded = self.store.put_secret_recorded(&sealed, event).await?;
        tracing::info!(secret = %name, "secret set");
        Ok((SecretVersion::from(&sealed), recorded))
    }

    /// `value` sealed as the secret `name`, with a fresh nonce, stamped now.
    fn seal(&self, name: &SecretName, value: &SecretValue) -> Result<SealedSecret> {
        let mut nonce = [0; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("the OS random number generator works");
        let ciphertext = self
            .cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: value.expose().as_bytes(),
                    aad: name.as_ref().as_bytes(),
                },
            )
            .map_err(|_| SecretError::Encrypt(name.to_string()))?;
        let now = Timestamp::now();
        Ok(SealedSecret {
            name: name.to_string(),
            nonce: nonce.to_vec(),
            ciphertext,
            created_at: now,
            updated_at: now,
        })
    }

    /// Removes the secret `name` without recording it. Returns whether it existed. Only tests
    /// write unrecorded (see [`set`](Self::set)).
    #[cfg(test)]
    pub async fn remove(&self, name: &SecretName) -> Result<bool> {
        let removed = self.store.remove_secret(name.as_ref()).await?;
        if removed {
            tracing::info!(secret = %name, "secret removed");
        }
        Ok(removed)
    }

    /// Removes the secret `name`, recording `event` in the event log in the same transaction
    /// (ADR-0055), if and only if the secret existed. `None` when it did not.
    pub async fn remove_recorded(
        &self,
        name: &SecretName,
        event: &NewEvent,
    ) -> Result<Option<Event>> {
        let recorded = self
            .store
            .remove_secret_recorded(name.as_ref(), event)
            .await?;
        if recorded.is_some() {
            tracing::info!(secret = %name, "secret removed");
        }
        Ok(recorded)
    }

    /// Every secret's name and last change, by name. Values are never listed.
    pub async fn list(&self) -> Result<Vec<SecretEntry>> {
        Ok(self.store.secret_entries().await?)
    }

    /// The current version of the secret `name`, or `None` if it is not set.
    pub async fn version(&self, name: &str) -> Result<Option<SecretVersion>> {
        Ok(self
            .store
            .sealed_secret(name)
            .await?
            .as_ref()
            .map(SecretVersion::from))
    }

    /// When the secret `name` was last set, or `None` if it is not set. Never its value.
    pub async fn changed_at(&self, name: &str) -> Result<Option<Timestamp>> {
        Ok(self
            .store
            .sealed_secret(name)
            .await?
            .map(|sealed| sealed.updated_at))
    }

    /// The plaintext and version of the secret `name`, or `None` if it is not set. For the
    /// deploy engine at container create only.
    pub async fn reveal(&self, name: &str) -> Result<Option<(SecretValue, SecretVersion)>> {
        let Some(sealed) = self.store.sealed_secret(name).await? else {
            return Ok(None);
        };
        let nonce: [u8; NONCE_LEN] = sealed
            .nonce
            .as_slice()
            .try_into()
            .map_err(|_| SecretError::Decrypt(name.to_owned()))?;
        let plaintext = self
            .cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &sealed.ciphertext,
                    aad: sealed.name.as_bytes(),
                },
            )
            .map_err(|_| SecretError::Decrypt(name.to_owned()))?;
        let value =
            String::from_utf8(plaintext).map_err(|_| SecretError::Decrypt(name.to_owned()))?;
        Ok(Some((SecretValue(value), SecretVersion::from(&sealed))))
    }
}

#[cfg(test)]
mod tests;
