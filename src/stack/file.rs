//! Config files a stack file carries for its services (ADR-0057): where a file goes in the
//! container ([`ContainerPath`]), what it holds ([`FileSource`]) and who may read it
//! ([`FileMode`], [`FileOwner`]).
//!
//! The types validate one value each. The rules between a file and the rest of its service
//! (other files, volumes) are checked at apply in `raw.rs`; the ones a deploy has to repeat on
//! a path resolved inside the container are [`ContainerPath::check_placement`].
//!
//! [`ContainerPath`] is also where a `tmpfs` mount goes (ADR-0059 §1), checked by
//! [`ContainerPath::check_tmpfs`].

use std::fmt;
use std::str::FromStr;

/// An absolute, normalized path inside a container: it starts with `/`, is not `/` itself, and
/// has no empty, `.` or `..` component, no trailing `/` and no NUL. Each location has exactly
/// one spelling, so two paths are the same file only when they are equal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContainerPath(String);

impl ContainerPath {
    /// Directories podman mounts a kernel or device file system over; nothing uploaded at or
    /// under them is seen by the container.
    const MOUNTED_OVER: [&str; 3] = ["/proc", "/sys", "/dev"];
    /// Files podman generates and mounts over the image's (the first four), or may append a
    /// line for the container's user to (the last two; architecture §1.7).
    const MANAGED: [&str; 6] = [
        "/etc/hosts",
        "/etc/hostname",
        "/etc/resolv.conf",
        "/run/.containerenv",
        "/etc/passwd",
        "/etc/group",
    ];

    /// Whether `inner` is `outer` or below it, on component boundaries.
    fn within(inner: &str, outer: &str) -> bool {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }

    /// Whether this path is a directory above `other`.
    pub fn is_ancestor_of(&self, other: &ContainerPath) -> bool {
        self != other && Self::within(&other.0, &self.0)
    }

    /// Whether a config file may be written here (ADR-0057 §2): not at or under `/proc`, `/sys`
    /// or `/dev`, not one of the files podman manages, and neither equal to, under nor a parent
    /// directory of one of `volumes` (mount paths of the service's or the image's volumes).
    ///
    /// Apply runs it on the declared path, and a deploy again on the path as the container
    /// resolves it, since a symlinked directory can lead into any of these.
    pub fn check_placement<'a>(
        &self,
        volumes: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), String> {
        self.check_not_mounted_over()?;
        if Self::MANAGED.contains(&self.0.as_str()) {
            return Err(format!(
                "`{self}` is written by podman itself and cannot be a config file"
            ));
        }
        for volume in volumes {
            // An image may declare its volume with a trailing `/`.
            let volume = match volume.trim_end_matches('/') {
                "" => "/",
                trimmed => trimmed,
            };
            if volume == self.0 {
                return Err(format!("`{self}` is the mount path of a volume"));
            }
            if volume == "/" || Self::within(&self.0, volume) {
                return Err(format!(
                    "`{self}` is in the volume mounted at `{volume}`; a volume hides files \
                     uploaded there"
                ));
            }
            if Self::within(volume, &self.0) {
                return Err(format!(
                    "`{self}` is a parent directory of the volume mounted at `{volume}`"
                ));
            }
        }
        Ok(())
    }

    /// Whether a tmpfs may be mounted here (ADR-0059 §1): not at or in `/proc`, `/sys` or
    /// `/dev`, and so not at `/dev/shm`, which `shm_size` sizes. Overlaps with the service's
    /// other mounts are [`ContainerPath::overlap`].
    pub fn check_tmpfs(&self) -> Result<(), String> {
        const SHM: &str = "/dev/shm";
        if Self::within(&self.0, SHM) {
            let place = if self.0 == SHM { "is" } else { "is in" };
            return Err(format!(
                "`{self}` {place} podman's `{SHM}`; set its size with `shm_size`"
            ));
        }
        self.check_not_mounted_over()
    }

    /// Not at or in one of [`Self::MOUNTED_OVER`].
    fn check_not_mounted_over(&self) -> Result<(), String> {
        match Self::MOUNTED_OVER
            .iter()
            .find(|dir| Self::within(&self.0, dir))
        {
            Some(dir) => {
                let place = if self.0 == **dir { "is" } else { "is in" };
                Err(format!(
                    "`{self}` {place} `{dir}`, which podman mounts over in every container"
                ))
            }
            None => Ok(()),
        }
    }

    /// How this path lies against `other`, another mount target or file path of the same
    /// container; `None` when neither is at or in the other.
    pub(super) fn overlap(&self, other: &str) -> Option<Overlap> {
        if self.0 == other {
            Some(Overlap::Same)
        } else if Self::within(&self.0, other) {
            Some(Overlap::Inside)
        } else if Self::within(other, &self.0) {
            Some(Overlap::Above)
        } else {
            None
        }
    }

    /// The directories above this path, outermost first, without `/`: `/etc` and `/etc/app`
    /// for `/etc/app/a.conf`.
    pub fn ancestors(&self) -> impl Iterator<Item = &str> {
        self.0
            .match_indices('/')
            .skip(1)
            .map(|(end, _)| &self.0[..end])
    }
}

/// How one container path lies against another ([`ContainerPath::overlap`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Overlap {
    /// They are the same path.
    Same,
    /// This path is below the other.
    Inside,
    /// This path is a directory above the other.
    Above,
}

impl FromStr for ContainerPath {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = |reason: &str| Err(format!("invalid path `{}`: {reason}", s.escape_debug()));
        let Some(rest) = s.strip_prefix('/') else {
            return invalid("must be an absolute path, starting with `/`");
        };
        if rest.is_empty() {
            return invalid("must name a file below `/`");
        }
        if s.contains('\0') {
            return invalid("must not contain a NUL character");
        }
        if rest.ends_with('/') {
            return invalid("must not end with `/`");
        }
        if rest
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
        {
            return invalid("must not have empty, `.` or `..` components");
        }
        Ok(Self(s.to_owned()))
    }
}

impl AsRef<str> for ContainerPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A file's permission bits, written as four octal digits from `"0000"` to `"0777"`. The
/// setuid, setgid and sticky bits cannot be set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileMode(u16);

impl FileMode {
    /// Inline content: anyone reads, nobody writes.
    pub const CONTENT: FileMode = FileMode(0o444);
    /// A secret: only its owner reads.
    pub const SECRET: FileMode = FileMode(0o400);
}

impl FromStr for FileMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.as_bytes() {
            [b'0', bits @ ..]
                if bits.len() == 3 && bits.iter().all(|b| (b'0'..=b'7').contains(b)) =>
            {
                Ok(Self(
                    bits.iter()
                        .fold(0, |mode, b| mode * 8 + u16::from(b - b'0')),
                ))
            }
            _ => Err(format!(
                "invalid mode `{}`: expected four octal digits from \"0000\" to \"0777\", e.g. \
                 \"0440\"",
                s.escape_debug()
            )),
        }
    }
}

impl fmt::Display for FileMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04o}", self.0)
    }
}

impl From<FileMode> for u32 {
    fn from(mode: FileMode) -> Self {
        u32::from(mode.0)
    }
}

/// A file's numeric owner inside the container, written `"uid:gid"`. Both ids are required and
/// at most 65535; names are not resolved (that would need the image's `/etc/passwd`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileOwner {
    pub uid: u32,
    pub gid: u32,
}

impl FileOwner {
    /// Root and its group.
    pub const ROOT: FileOwner = FileOwner { uid: 0, gid: 0 };
    /// The largest id the format accepts. Whether an id is usable depends on the host's
    /// subuid/subgid range, which a deploy finds out.
    pub const MAX_ID: u32 = 65535;

    fn id(text: &str) -> Option<u32> {
        let digits = (1..=5).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit());
        let id = digits.then(|| text.parse::<u32>().ok()).flatten()?;
        (id <= Self::MAX_ID).then_some(id)
    }
}

impl FromStr for FileOwner {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.split_once(':')
            .and_then(|(uid, gid)| {
                Some(Self {
                    uid: Self::id(uid)?,
                    gid: Self::id(gid)?,
                })
            })
            .ok_or_else(|| {
                format!(
                    "invalid owner `{}`: expected numeric `uid:gid`, each at most {}, e.g. \
                     \"0:65534\"",
                    s.escape_debug(),
                    Self::MAX_ID
                )
            })
    }
}

impl fmt::Display for FileOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

/// Who owns a config file once its defaults are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// The ids the stack file gives, or root for inline content.
    Ids(FileOwner),
    /// The user the container runs as (the stack's `user`, else the image's), with the group
    /// podman gives it. The default for secret files; only podman knows the ids.
    ContainerUser,
}

/// `uid:gid`, or `user` for the container's user.
impl fmt::Display for Ownership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ownership::Ids(owner) => owner.fmt(f),
            Ownership::ContainerUser => f.write_str("user"),
        }
    }
}

/// What a config file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSource {
    /// Text from the stack file itself.
    Inline(String),
    /// The current value of a secret of the same stack, `<stack>/<name>` (ADR-0029).
    Secret(String),
}

/// One config file of a service (ADR-0057): hoster writes it into each container of the
/// service before the container starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    pub source: FileSource,
    /// The mode the stack file sets; [`ConfigFile::resolved_mode`] applies the default.
    pub mode: Option<FileMode>,
    /// The owner the stack file sets; [`ConfigFile::ownership`] applies the default.
    pub owner: Option<FileOwner>,
}

impl ConfigFile {
    /// The most inline content one file may hold, in bytes (256 KiB). A secret file's value is
    /// held to the same limit when it is deployed.
    pub const MAX_BYTES: usize = 256 * 1024;
    /// The most inline content all files of a stack may hold together, in bytes (1 MiB).
    /// Secret files do not count.
    pub const MAX_STACK_BYTES: usize = 1024 * 1024;

    /// The file's mode: the declared one, else `0444` for inline content and `0400` for a
    /// secret.
    pub fn resolved_mode(&self) -> FileMode {
        self.mode.unwrap_or(match self.source {
            FileSource::Inline(_) => FileMode::CONTENT,
            FileSource::Secret(_) => FileMode::SECRET,
        })
    }

    /// The file's owner: the declared one, else root for inline content and the container's
    /// user for a secret.
    pub fn ownership(&self) -> Ownership {
        match (self.owner, &self.source) {
            (Some(owner), _) => Ownership::Ids(owner),
            (None, FileSource::Inline(_)) => Ownership::Ids(FileOwner::ROOT),
            (None, FileSource::Secret(_)) => Ownership::ContainerUser,
        }
    }
}
