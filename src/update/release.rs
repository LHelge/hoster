//! What a release is: its version, notes and files, and the `SHA256SUMS` that vouches for them.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::SystemTime;
use thiserror::Error;

use crate::installation::Version;

/// A published hoster release (ADR-0052).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The release notes (git-cliff Markdown).
    pub notes: String,
    pub assets: Vec<Asset>,
    /// When it was published, if the source says.
    pub published: Option<SystemTime>,
    /// The release's page on GitHub (its full notes), if the source names one on github.com.
    pub page: Option<Url>,
}

impl Release {
    /// The checksum file every release carries.
    pub const CHECKSUMS: &str = "SHA256SUMS";

    /// The binary asset for `target`: `hoster-<target>`.
    pub fn binary_name(target: &str) -> String {
        format!("hoster-{target}")
    }

    /// The asset named `name`.
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// One file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: Url,
    /// The size the source announced, if any.
    pub size: Option<u64>,
}

/// Errors from a [`ReleaseSource`].
#[derive(Debug, Error)]
pub enum SourceError {
    #[error("cannot reach GitHub ({url}); check the host's network and DNS and try again")]
    Request {
        url: Url,
        #[source]
        source: reqwest::Error,
    },
    #[error("timed out talking to {url}; try again later")]
    Timeout { url: Url },
    #[error("{url} answered {status}")]
    Status { url: Url, status: http::StatusCode },
    #[error("GitHub's API rate limit for this host is used up ({url}); try again in an hour")]
    RateLimited { url: Url },
    #[error("there is no hoster release {0} on GitHub; check the version on the releases page")]
    UnknownRelease(Version),
    #[error("no stable hoster release is published yet")]
    NoRelease,
    #[error("GitHub returned {0} as the latest release, but it is a pre-release or draft")]
    NotStable(Version),
    #[error("GitHub returned an unexpected release: {0}")]
    Malformed(String),
    #[error("refusing to fetch {url}: only https URLs on GitHub's release hosts are followed")]
    UntrustedUrl { url: Url },
    #[error("{url} is larger than the {limit} bytes allowed")]
    TooLarge { url: Url, limit: u64 },
    #[error("cannot write the download to {}", path.display())]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Where releases come from: GitHub in production
/// ([`GithubReleases`](super::GithubReleases)), a fake in tests.
#[async_trait]
pub trait ReleaseSource: Send + Sync {
    /// The newest stable release; never a pre-release.
    async fn latest(&self) -> Result<Release, SourceError>;
    /// The release `version`, pre-release or not.
    async fn release(&self, version: &Version) -> Result<Release, SourceError>;
    /// Downloads `asset` to the new file `dest`, failing once it exceeds `limit` bytes. A
    /// failed download may leave a partial `dest`; the caller removes it.
    async fn download(&self, asset: &Asset, limit: u64, dest: &Path) -> Result<(), SourceError>;
}

/// A SHA-256 digest, written as 64 lowercase hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// The digest of the file at `path`.
    pub fn of_file(path: &Path) -> std::io::Result<Self> {
        use std::io::Read;
        let mut hasher = Sha256::new();
        let mut file = std::fs::File::open(path)?;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match file.read(&mut buf)? {
                0 => break,
                n => hasher.update(&buf[..n]),
            }
        }
        Ok(Self(hasher.finalize().into()))
    }
}

impl From<&[u8]> for Sha256Digest {
    fn from(data: &[u8]) -> Self {
        Self(Sha256::digest(data).into())
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl FromStr for Sha256Digest {
    type Err = ChecksumsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || ChecksumsError(format!("{s:?} is not a SHA-256 hex digest"));
        if s.len() != 64 || !s.is_ascii() {
            return Err(bad());
        }
        let mut out = [0u8; 32];
        for (byte, pair) in out.iter_mut().zip(s.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| bad())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| bad())?;
        }
        Ok(Self(out))
    }
}

/// A malformed `SHA256SUMS` file.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("malformed SHA256SUMS: {0}")]
pub struct ChecksumsError(String);

/// A parsed `SHA256SUMS` file as `sha256sum` writes it: `<hex>  <name>` per line (`*<name>`
/// in binary mode).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Checksums(BTreeMap<String, Sha256Digest>);

impl Checksums {
    /// The digest listed for `name`.
    pub fn get(&self, name: &str) -> Option<Sha256Digest> {
        self.0.get(name).copied()
    }
}

impl FromStr for Checksums {
    type Err = ChecksumsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut sums = BTreeMap::new();
        for line in s.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
            let (digest, name) = line
                .split_once(' ')
                .ok_or_else(|| ChecksumsError(format!("line {line:?} has no file name")))?;
            let name = name.trim_start_matches(' ');
            let name = name.strip_prefix('*').unwrap_or(name);
            if name.is_empty() {
                return Err(ChecksumsError(format!("line {line:?} has no file name")));
            }
            // `sha256sum` may list a path; release assets are flat.
            let name = name.rsplit('/').next().unwrap_or(name);
            if sums.insert(name.to_owned(), digest.parse()?).is_some() {
                return Err(ChecksumsError(format!("{name} is listed twice")));
            }
        }
        Ok(Self(sums))
    }
}
