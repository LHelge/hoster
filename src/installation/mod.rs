//! The versioned binary layout (ADR-0052) and the facts a binary reports about itself.
//!
//! ```text
//! ~/.local/lib/hoster/
//!   versions/vX.Y.Z/hoster
//!   current -> versions/vX.Y.Z
//! ```
//!
//! The unit runs `%h/.local/lib/hoster/current/hoster run`. [`Installation`] is the only code
//! that touches this tree: it lists the installed versions, finds the current and previous one,
//! installs a binary as a new version (temp file, fsync, `chmod 0755`, `rename`), switches
//! `current` atomically (a new symlink `rename(2)`d over the old one) and prunes everything but
//! the current and previous version. [`BuildInfo`] is what `hoster version --json` prints: the
//! version, target triple and schema of the running binary, which `hoster rollback` reads from
//! the previous binary.

#[cfg(test)]
mod tests;

use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::XdgDirs;
use crate::store::Store;

/// Errors from reading or changing the binary layout.
#[derive(Debug, Error)]
pub enum InstallationError {
    #[error("{}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{version} is not installed under {}", root.display())]
    NotInstalled { version: Version, root: PathBuf },
    #[error("{} does not point at an installed version", link.display())]
    NoCurrent { link: PathBuf },
    #[error("{} points outside {}: {}", link.display(), versions.display(), target.display())]
    ForeignCurrent {
        link: PathBuf,
        versions: PathBuf,
        target: PathBuf,
    },
    #[error("{version} is the current version and cannot be removed")]
    #[allow(dead_code, reason = "used by hoster update (ADR-0052)")]
    RemovingCurrent { version: Version },
    #[allow(dead_code, reason = "used by hoster rollback (ADR-0052)")]
    #[error(
        "cannot ask {} for its version (`hoster version --json`): {reason}; \
         an older binary may not support it, see the manual rollback in docs/install.md",
        binary.display()
    )]
    Probe { binary: PathBuf, reason: String },
}

type Result<T, E = InstallationError> = std::result::Result<T, E>;

/// Attaches the path an I/O error is about.
trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| InstallationError::Io {
            path: path.to_owned(),
            source,
        })
    }
}

/// A hoster release version. It displays as the directory name, `vX.Y.Z`; it parses with or
/// without the `v`. Ordering is SemVer precedence, so `v0.2.0-rc.1` < `v0.2.0`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(semver::Version);

impl Version {
    /// The version of the running binary (`Cargo.toml`).
    pub fn running() -> Self {
        env!("CARGO_PKG_VERSION")
            .parse()
            .expect("CARGO_PKG_VERSION is SemVer")
    }

    /// Whether this is a pre-release (`-rc.1`), installed only when named explicitly.
    #[allow(dead_code, reason = "used by hoster update (ADR-0052)")]
    pub fn is_prerelease(&self) -> bool {
        !self.0.pre.is_empty()
    }
}

impl FromStr for Version {
    type Err = semver::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.strip_prefix('v').unwrap_or(s).parse().map(Self)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

impl From<semver::Version> for Version {
    fn from(version: semver::Version) -> Self {
        Self(version)
    }
}

impl AsRef<semver::Version> for Version {
    fn as_ref(&self) -> &semver::Version {
        &self.0
    }
}

/// What a binary reports about itself (`hoster version --json`, ADR-0052).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    /// `X.Y.Z`, without the `v`.
    pub version: semver::Version,
    /// The target triple it was built for.
    pub target: String,
    /// The newest migration it contains: the schema it runs against.
    pub schema: i64,
}

impl BuildInfo {
    /// The running binary.
    pub fn running() -> Self {
        Self {
            version: Version::running().0,
            target: env!("BUILD_TARGET_TRIPLE").to_owned(),
            schema: Store::expected_schema_version().into(),
        }
    }
}

#[allow(dead_code, reason = "used by hoster rollback (ADR-0052)")]
impl BuildInfo {
    /// How long a binary may take to answer `hoster version --json`.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
    /// Attempts at starting a binary that is still busy being written (`ETXTBSY`).
    const PROBE_ATTEMPTS: u32 = 5;

    /// Asks the binary at `binary` what it is by running `<binary> version --json` (ADR-0052:
    /// `hoster rollback` reads the previous binary's schema this way).
    pub async fn probe(binary: &Path) -> Result<Self> {
        let fail = |reason: String| InstallationError::Probe {
            binary: binary.to_owned(),
            reason,
        };
        let mut attempt = 1;
        let output = loop {
            let run = tokio::process::Command::new(binary)
                .args(["version", "--json"])
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output();
            match tokio::time::timeout(Self::PROBE_TIMEOUT, run).await {
                Err(_) => return Err(fail(format!("no answer in {:?}", Self::PROBE_TIMEOUT))),
                // A file just written may still be open in a child forked meanwhile.
                Ok(Err(e))
                    if e.kind() == io::ErrorKind::ExecutableFileBusy
                        && attempt < Self::PROBE_ATTEMPTS =>
                {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Ok(Err(e)) => return Err(fail(e.to_string())),
                Ok(Ok(output)) => break output,
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(fail(format!("{}: {}", output.status, stderr.trim())));
        }
        serde_json::from_slice(&output.stdout).map_err(|e| fail(format!("unexpected output: {e}")))
    }
}

impl fmt::Display for BuildInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "hoster {} ({}, schema {})",
            self.version, self.target, self.schema
        )
    }
}

/// The binary layout rooted at `~/.local/lib/hoster` (ADR-0052).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    root: PathBuf,
}

impl Installation {
    /// The binary inside each version directory.
    const BINARY: &str = "hoster";
    const VERSIONS: &str = "versions";
    const CURRENT: &str = "current";

    /// The layout under `$HOME/.local/lib/hoster`; `None` when `$HOME` is unset.
    pub fn from_env() -> Option<Self> {
        XdgDirs::from_env().binary_root().map(Self::new)
    }

    /// The layout rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `current` symlink.
    pub fn current_link(&self) -> PathBuf {
        self.root.join(Self::CURRENT)
    }

    /// Where `version`'s binary lives (whether or not it is installed).
    pub fn binary(&self, version: &Version) -> PathBuf {
        self.version_dir(version).join(Self::BINARY)
    }

    fn versions_dir(&self) -> PathBuf {
        self.root.join(Self::VERSIONS)
    }

    fn version_dir(&self, version: &Version) -> PathBuf {
        self.versions_dir().join(version.to_string())
    }

    /// The version a directory under `versions/` stands for. Only the exact name
    /// [`version_dir`](Self::version_dir) gives counts: `1.2.3` (no `v`) is a stray, not
    /// `v1.2.3`.
    fn version_named(name: &OsStr) -> Option<Version> {
        let name = name.to_str()?;
        let version: Version = name.parse().ok()?;
        (version.to_string() == name).then_some(version)
    }

    fn is_installed(&self, version: &Version) -> bool {
        self.binary(version).is_file()
    }

    /// The installed versions, oldest first. A directory counts when its name is `v<semver>`
    /// and it holds a `hoster` file; anything else under `versions/` is ignored.
    pub fn versions(&self) -> Result<Vec<Version>> {
        let dir = self.versions_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).at(&dir),
        };
        let mut versions = Vec::new();
        for entry in entries {
            let entry = entry.at(&dir)?;
            let Some(version) = Self::version_named(&entry.file_name()) else {
                continue;
            };
            if self.is_installed(&version) {
                versions.push(version);
            }
        }
        versions.sort();
        Ok(versions)
    }

    /// The version `current` points at; `None` when the link is missing or dangling.
    pub fn current(&self) -> Result<Option<Version>> {
        let link = self.current_link();
        let target = match fs::read_link(&link) {
            Ok(target) => target,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).at(&link),
        };
        let resolved = self.root.join(&target);
        let foreign = || InstallationError::ForeignCurrent {
            link: link.clone(),
            versions: self.versions_dir(),
            target: target.clone(),
        };
        if resolved.parent() != Some(self.versions_dir().as_path()) {
            return Err(foreign());
        }
        let version = resolved
            .file_name()
            .and_then(Self::version_named)
            .ok_or_else(foreign)?;
        Ok(self.is_installed(&version).then_some(version))
    }

    /// The newest installed version older than the current one: what `hoster rollback` returns
    /// to. `None` without a current version or an older one.
    #[allow(dead_code, reason = "used by hoster rollback (ADR-0052)")]
    pub fn previous(&self) -> Result<Option<Version>> {
        let Some(current) = self.current()? else {
            return Ok(None);
        };
        Ok(self.versions()?.into_iter().rfind(|v| *v < current))
    }

    /// Installs the binary at `source` as `version`: it is copied to a temporary file next to
    /// its final place, fsynced, made `0755` and renamed into place, so a crash never leaves a
    /// partial `hoster` behind. Installing a version again replaces its binary the same way.
    /// Returns the installed binary's path.
    #[allow(dead_code, reason = "used by hoster update (ADR-0052)")]
    pub fn install(&self, version: &Version, source: &Path) -> Result<PathBuf> {
        let dir = self.version_dir(version);
        fs::create_dir_all(&dir).at(&dir)?;
        let dest = dir.join(Self::BINARY);
        let temp = dir.join(format!(".{}.tmp-{}", Self::BINARY, std::process::id()));
        let written =
            Self::write_executable(source, &temp).and_then(|()| fs::rename(&temp, &dest).at(&dest));
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written?;
        // The binary's entry, then the directories create_dir_all may just have made.
        Self::sync_dir(&dir)?;
        Self::sync_dir(&self.versions_dir())?;
        Self::sync_dir(&self.root)?;
        Ok(dest)
    }

    fn write_executable(source: &Path, temp: &Path) -> Result<()> {
        let mut from = File::open(source).at(source)?;
        let mut to = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(temp)
            .at(temp)?;
        io::copy(&mut from, &mut to).at(temp)?;
        // The mode given to open() is masked by the umask (0077 in the unit).
        to.set_permissions(Permissions::from_mode(0o755)).at(temp)?;
        to.sync_all().at(temp)
    }

    /// Points `current` at `version` atomically: a new relative symlink is created next to it
    /// and `rename(2)`d over it, so `current` always names some version.
    #[allow(dead_code, reason = "used by hoster update and rollback (ADR-0052)")]
    pub fn switch(&self, version: &Version) -> Result<()> {
        if !self.is_installed(version) {
            return Err(InstallationError::NotInstalled {
                version: version.clone(),
                root: self.root.clone(),
            });
        }
        let link = self.current_link();
        let temp = self
            .root
            .join(format!(".{}.tmp-{}", Self::CURRENT, std::process::id()));
        let _ = fs::remove_file(&temp);
        let target = Path::new(Self::VERSIONS).join(version.to_string());
        std::os::unix::fs::symlink(&target, &temp).at(&temp)?;
        if let Err(e) = fs::rename(&temp, &link) {
            let _ = fs::remove_file(&temp);
            return Err(e).at(&link);
        }
        Self::sync_dir(&self.root)
    }

    /// Deletes every installed version but the current and the previous one. Returns the
    /// deleted versions, oldest first. Refuses (deleting nothing) without a current version.
    #[allow(dead_code, reason = "used by hoster update (ADR-0052)")]
    pub fn prune(&self) -> Result<Vec<Version>> {
        let current = self
            .current()?
            .ok_or_else(|| InstallationError::NoCurrent {
                link: self.current_link(),
            })?;
        let previous = self.previous()?;
        let mut removed = Vec::new();
        for version in self.versions()? {
            if version == current || Some(&version) == previous.as_ref() {
                continue;
            }
            let dir = self.version_dir(&version);
            fs::remove_dir_all(&dir).at(&dir)?;
            removed.push(version);
        }
        if !removed.is_empty() {
            Self::sync_dir(&self.versions_dir())?;
        }
        Ok(removed)
    }

    /// Deletes `version`'s directory; nothing happens when it is not installed. Refuses to
    /// delete the current version. Undoes an install that is not switched to.
    #[allow(dead_code, reason = "used by hoster update (ADR-0052)")]
    pub fn uninstall(&self, version: &Version) -> Result<()> {
        if self.current()?.as_ref() == Some(version) {
            return Err(InstallationError::RemovingCurrent {
                version: version.clone(),
            });
        }
        let dir = self.version_dir(version);
        match fs::remove_dir_all(&dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            other => other.at(&dir)?,
        }
        Self::sync_dir(&self.versions_dir())
    }

    /// The installed version `exe` belongs to, when it is a binary inside this layout (through
    /// `current` or `versions/`); `None` for a binary installed anywhere else.
    pub fn version_of(&self, exe: &Path) -> Option<Version> {
        let exe = fs::canonicalize(exe).ok()?;
        let versions = fs::canonicalize(self.versions_dir()).ok()?;
        let relative = exe.strip_prefix(&versions).ok()?;
        let mut parts = relative.components();
        let version = Self::version_named(parts.next()?.as_os_str())?;
        let binary = parts.next()?;
        (binary.as_os_str() == Self::BINARY && parts.next().is_none()).then_some(version)
    }

    fn sync_dir(dir: &Path) -> Result<()> {
        File::open(dir).and_then(|d| d.sync_all()).at(dir)
    }
}
