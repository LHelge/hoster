//! The instance name (ADR-0058 §2): a short display label for this hoster, shown in the sidebar
//! and in every notification mail's subject and footer. It is a label only; nothing is derived
//! from it.
//!
//! It is the first run-time setting (ADR-0058 §1): it lives in SQLite, not in `hoster.toml`, is
//! edited in Settings → General and takes effect without a restart. Without a stored name it is
//! the machine's hostname, which is what the sidebar showed before the setting existed.
//!
//! [`Instance`] is the one accessor: it reads the stored name once, keeps it in memory and
//! refreshes it on every change, so the sidebar and the notifier read it without a query.

use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use thiserror::Error;

use crate::store::{Store, StoreError};

/// A validated instance name: trimmed, non-empty, at most [`InstanceName::MAX_CHARS`]
/// characters and free of control characters and line breaks, since it goes into a mail
/// subject.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstanceName(String);

/// Why a text is not an [`InstanceName`]. The messages are shown in Settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InvalidInstanceName {
    #[error("the instance name is empty")]
    Empty,
    #[error(
        "the instance name is longer than {} characters",
        InstanceName::MAX_CHARS
    )]
    TooLong,
    #[error("the instance name contains a line break or another control character")]
    ControlCharacter,
}

impl InstanceName {
    /// The longest name, in characters; also the limit of a Linux hostname.
    pub const MAX_CHARS: usize = 64;

    /// The name used when the hostname cannot be read or is not a valid name.
    const FALLBACK: &str = "hoster";

    /// A line break or another character that must not reach a mail header.
    fn is_forbidden(c: char) -> bool {
        c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')
    }
}

impl FromStr for InstanceName {
    type Err = InvalidInstanceName;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let name = raw.trim();
        if name.is_empty() {
            return Err(InvalidInstanceName::Empty);
        }
        if name.chars().count() > Self::MAX_CHARS {
            return Err(InvalidInstanceName::TooLong);
        }
        if name.chars().any(Self::is_forbidden) {
            return Err(InvalidInstanceName::ControlCharacter);
        }
        Ok(Self(name.to_owned()))
    }
}

/// The default name: the machine's hostname, or `hoster` when it cannot be read.
impl Default for InstanceName {
    fn default() -> Self {
        nix::unistd::gethostname()
            .ok()
            .and_then(|name| name.into_string().ok())
            .and_then(|name| name.parse().ok())
            .unwrap_or_else(|| Self(Self::FALLBACK.to_owned()))
    }
}

impl fmt::Display for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for InstanceName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// The instance name as hoster uses it: the stored name, or the default. Cheap to clone; clones
/// share the cached name, so a change made through one is seen by all.
#[derive(Debug, Clone)]
pub struct Instance(Arc<Shared>);

#[derive(Debug)]
struct Shared {
    store: Store,
    /// The name without a stored one: the hostname, read once.
    default: InstanceName,
    /// The stored name, mirrored from the store.
    stored: RwLock<Option<InstanceName>>,
    /// Serialises changes, so the cache ends up as the last write left the store.
    writes: tokio::sync::Mutex<()>,
}

impl Instance {
    /// The instance name kept in `store`, defaulting to the machine's hostname.
    pub async fn load(store: Store) -> Result<Self, StoreError> {
        Self::with_default(store, InstanceName::default()).await
    }

    /// The instance name kept in `store`, defaulting to `default`.
    pub async fn with_default(store: Store, default: InstanceName) -> Result<Self, StoreError> {
        let stored = store.instance_name().await?;
        Ok(Self(Arc::new(Shared {
            store,
            default,
            stored: RwLock::new(stored),
            writes: tokio::sync::Mutex::new(()),
        })))
    }

    /// The current name: the stored one, or the default. No store read.
    pub fn name(&self) -> InstanceName {
        self.stored().unwrap_or_else(|| self.0.default.clone())
    }

    /// The name set by the operator, if any.
    pub fn stored(&self) -> Option<InstanceName> {
        self.0
            .stored
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The name used without a stored one: the hostname.
    pub fn default_name(&self) -> &InstanceName {
        &self.0.default
    }

    /// Stores `name`; it is the instance name from now on.
    pub async fn set(&self, name: &InstanceName) -> Result<(), StoreError> {
        let _write = self.0.writes.lock().await;
        self.0.store.set_instance_name(Some(name)).await?;
        self.cache(Some(name.clone()));
        Ok(())
    }

    /// Forgets the stored name, so the instance name is the default again.
    pub async fn reset(&self) -> Result<(), StoreError> {
        let _write = self.0.writes.lock().await;
        self.0.store.set_instance_name(None).await?;
        self.cache(None);
        Ok(())
    }

    fn cache(&self, name: Option<InstanceName>) {
        *self
            .0
            .stored
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = name;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(raw: &str) -> Result<String, InvalidInstanceName> {
        raw.parse::<InstanceName>().map(|name| name.to_string())
    }

    #[test]
    fn names_are_trimmed_and_checked() {
        assert_eq!(name("  srv-01 \t").as_deref(), Ok("srv-01"));
        assert_eq!(
            name("Prod <EU> & \"main\"").as_deref(),
            Ok("Prod <EU> & \"main\"")
        );
        assert_eq!(name("blåbär 🫐").as_deref(), Ok("blåbär 🫐"));
        assert_eq!(name(""), Err(InvalidInstanceName::Empty));
        assert_eq!(name(" \n "), Err(InvalidInstanceName::Empty));
        assert_eq!(name("a\nb"), Err(InvalidInstanceName::ControlCharacter));
        assert_eq!(name("a\r\nb"), Err(InvalidInstanceName::ControlCharacter));
        assert_eq!(name("a\tb"), Err(InvalidInstanceName::ControlCharacter));
        assert_eq!(name("a\u{7}b"), Err(InvalidInstanceName::ControlCharacter));
        assert_eq!(
            name("a\u{2028}b"),
            Err(InvalidInstanceName::ControlCharacter)
        );

        let longest = "å".repeat(InstanceName::MAX_CHARS);
        assert_eq!(name(&longest).as_deref(), Ok(longest.as_str()));
        assert_eq!(
            name(&format!("{longest}x")),
            Err(InvalidInstanceName::TooLong)
        );
    }

    #[test]
    fn the_default_is_a_valid_name() {
        let default = InstanceName::default();
        assert_eq!(
            default.as_ref().parse::<InstanceName>(),
            Ok(default.clone())
        );
    }

    #[tokio::test]
    async fn changes_are_stored_and_seen_by_every_clone() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let default: InstanceName = "srv-01".parse().unwrap();
        let instance = Instance::with_default(store.clone(), default.clone())
            .await
            .unwrap();
        let other = instance.clone();
        assert_eq!(instance.name(), default);
        assert_eq!(instance.stored(), None);

        let prod: InstanceName = "prod".parse().unwrap();
        instance.set(&prod).await.unwrap();
        assert_eq!(other.name(), prod);
        assert_eq!(other.stored(), Some(prod.clone()));
        // A fresh load reads it back.
        let reloaded = Instance::with_default(store.clone(), default.clone())
            .await
            .unwrap();
        assert_eq!(reloaded.name(), prod);

        other.reset().await.unwrap();
        assert_eq!(instance.name(), default);
        assert_eq!(instance.default_name(), &default);
        let reloaded = Instance::with_default(store, default.clone())
            .await
            .unwrap();
        assert_eq!(reloaded.stored(), None);
    }
}
