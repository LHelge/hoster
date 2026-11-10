//! Config files written into a new container before it starts (ADR-0057 §3 and §4,
//! docs/architecture.md §1.7 and §8).
//!
//! [`ContainerFile`] is what a [`ContainerSpec`](super::ContainerSpec) carries. The rest is the
//! part of the upload that needs no podman: reading podman's answer about a path
//! ([`PathStat`]), working out where each file really goes once the image's symbolic links are
//! followed ([`Layout::resolve`]), packing the tar archives ([`Layout::archives`]) and reducing
//! podman's error to what may be shown ([`UploadFailure`]). `podman.rs` does the HTTP.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::io;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

use super::RuntimeError;
use crate::secrets::SecretValue;
use crate::stack::{ContainerPath, FileMode, FileOwner, Ownership};

/// One config file of a container: its path, content, mode and owner, all resolved by the
/// engine. Nothing on the host backs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerFile {
    pub path: ContainerPath,
    /// The file's text. Held as a [`SecretValue`] whether it came from the stack file or the
    /// secret store, so nothing that prints a spec shows a file's content.
    pub content: SecretValue,
    pub mode: FileMode,
    pub owner: Ownership,
}

/// What podman's `HEAD /containers/{id}/archive?path=` says about a path (architecture §1.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PathStat {
    /// Nothing is there.
    Missing,
    /// A symbolic link to `target`, where nothing is.
    Dangling { target: String },
    Found {
        is_dir: bool,
        /// Whether the path itself is a symbolic link.
        is_symlink: bool,
        /// The path with every symbolic link followed; `/` for the root.
        resolved: String,
    },
}

/// The JSON in the `X-Docker-Container-Path-Stat` header.
#[derive(Deserialize)]
struct RawStat {
    #[serde(default)]
    mode: u32,
    #[serde(rename = "isDir", default)]
    is_dir: bool,
    #[serde(rename = "linkTarget", default)]
    link_target: String,
}

impl PathStat {
    /// The header carrying the stat, base64 JSON.
    pub const HEADER: &str = "x-docker-container-path-stat";
    /// Go's `os.ModeSymlink`, as podman reports a link's mode.
    const MODE_SYMLINK: u32 = 1 << 27;

    /// Reads the answer to a stat of `path`: the HTTP status and the stat header, if any.
    /// A 404 without the header is a missing path, one with it a dangling link.
    pub fn from_response(path: &str, status: u16, header: Option<&str>) -> Result<Self, String> {
        let stat = header
            .map(|header| {
                let json = BASE64
                    .decode(header.trim())
                    .map_err(|err| format!("unreadable stat of `{path}`: {err}"))?;
                serde_json::from_slice::<RawStat>(&json)
                    .map_err(|err| format!("unreadable stat of `{path}`: {err}"))
            })
            .transpose()?;
        match (status, stat) {
            (404, None) => Ok(Self::Missing),
            (404, Some(stat)) => Ok(Self::Dangling {
                target: stat.link_target,
            }),
            (200, Some(stat)) => {
                let resolved = Self::normalized(&stat.link_target);
                Ok(Self::Found {
                    is_dir: stat.is_dir,
                    // Not `resolved != path`: that also holds below a linked directory.
                    is_symlink: stat.mode & Self::MODE_SYMLINK != 0,
                    resolved,
                })
            }
            (status, _) => Err(format!("podman answered {status} to a stat of `{path}`")),
        }
    }

    /// podman writes the root as `/.`.
    fn normalized(target: &str) -> String {
        let trimmed = target.trim_end_matches("/.").trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_owned()
        } else {
            trimmed.to_owned()
        }
    }
}

/// Something that can stat a path inside one container: podman, or a test's table.
pub(super) trait StatPath: Sync {
    fn stat(&self, path: &str) -> impl Future<Output = Result<PathStat, RuntimeError>> + Send;
}

/// Stats each path once: the files of one service share most of their directories.
struct Lookup<'p, P> {
    paths: &'p P,
    seen: BTreeMap<String, PathStat>,
}

impl<P: StatPath> Lookup<'_, P> {
    async fn stat(&mut self, path: &str) -> Result<PathStat, RuntimeError> {
        if let Some(known) = self.seen.get(path) {
            return Ok(known.clone());
        }
        let found = self.paths.stat(path).await?;
        self.seen.insert(path.to_owned(), found.clone());
        Ok(found)
    }
}

/// Where a container's config files go once the image's symbolic links are followed, and the
/// directories that have to be created for them.
#[derive(Debug, Default)]
pub(super) struct Layout<'a> {
    /// Missing parent directories, by resolved path. Sorted, so a parent precedes its children.
    dirs: BTreeSet<String>,
    /// The files, by resolved path.
    files: BTreeMap<String, &'a ContainerFile>,
}

/// The two uploads of a [`Layout`]; each is a tar archive rooted at `/`, or `None` when it
/// would be empty.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Archives {
    /// Missing parent directories (`0:0 0755`) and the files with numeric owners. Sent first.
    pub ids: Option<Vec<u8>>,
    /// The files owned by the container's user: sent with `copyUIDGID=true`, which gives every
    /// entry, and every directory it would create, to that user (hence the directories above).
    pub container_user: Option<Vec<u8>>,
}

impl<'a> Layout<'a> {
    /// Resolves `files` against the container's file system through `paths`, and checks each
    /// resolved path again (ADR-0057 §3): it may not be a symbolic link or a directory, hang
    /// below a file or a dangling link, or land at a reserved path or in or above one of
    /// `volumes` (the mount paths of the service's and the image's volumes).
    pub async fn resolve(
        files: &'a [ContainerFile],
        volumes: &[String],
        paths: &impl StatPath,
    ) -> Result<Self, RuntimeError> {
        let refuse = |message: String| RuntimeError::ConfigFiles { message };
        let mut layout = Self::default();
        let mut lookup = Lookup {
            paths,
            seen: BTreeMap::new(),
        };

        for file in files {
            let declared = file.path.as_ref();
            let (resolved, dirs) = match lookup.stat(declared).await? {
                PathStat::Dangling { target }
                | PathStat::Found {
                    is_symlink: true,
                    resolved: target,
                    ..
                } => {
                    return Err(refuse(format!(
                        "`{declared}` is a symbolic link in the image (to `{target}`); a config \
                         file cannot replace a link"
                    )));
                }
                PathStat::Found { is_dir: true, .. } => {
                    return Err(refuse(format!("`{declared}` is a directory in the image")));
                }
                PathStat::Found { resolved, .. } => (resolved, Vec::new()),
                PathStat::Missing => {
                    let ancestors: Vec<&str> = file.path.ancestors().collect();
                    // The deepest ancestor that exists, as (how much of `declared` it covers,
                    // where it really is); the root when none does.
                    let mut base = (0, String::new());
                    for ancestor in ancestors.iter().rev() {
                        match lookup.stat(ancestor).await? {
                            PathStat::Missing => {}
                            PathStat::Dangling { target } => {
                                return Err(refuse(format!(
                                    "`{declared}` is below `{ancestor}`, a symbolic link in \
                                     the image to `{target}`, which does not exist"
                                )));
                            }
                            PathStat::Found { is_dir: false, .. } => {
                                return Err(refuse(format!(
                                    "`{declared}` is below `{ancestor}`, which is a file in the \
                                     image"
                                )));
                            }
                            PathStat::Found { resolved, .. } => {
                                let real = if resolved == "/" {
                                    String::new()
                                } else {
                                    resolved
                                };
                                base = (ancestor.len(), real);
                                break;
                            }
                        }
                    }
                    let (covered, real) = base;
                    let dirs = ancestors
                        .iter()
                        .filter(|ancestor| ancestor.len() > covered)
                        .map(|ancestor| format!("{real}{}", &ancestor[covered..]))
                        .collect();
                    (format!("{real}{}", &declared[covered..]), dirs)
                }
            };

            let place: ContainerPath = resolved.parse().map_err(|_| {
                refuse(format!(
                    "`{declared}` is `{resolved}` in the image, which is no place for a file"
                ))
            })?;
            place
                .check_placement(volumes.iter().map(String::as_str))
                .map_err(|reason| {
                    refuse(if resolved == declared {
                        reason
                    } else {
                        format!("`{declared}` is `{resolved}` in the image: {reason}")
                    })
                })?;
            if let Some(other) = layout.files.insert(resolved.clone(), file) {
                return Err(refuse(format!(
                    "`{}` and `{declared}` are the same file in the image, `{resolved}`",
                    other.path
                )));
            }
            layout.dirs.extend(dirs);
        }
        // Declared paths never nest (ADR-0057 §2), but two can meet through a link.
        if let Some((resolved, file)) = layout
            .files
            .iter()
            .find(|(resolved, _)| layout.dirs.contains(*resolved))
        {
            return Err(refuse(format!(
                "`{}` is `{resolved}` in the image, which another config file needs as a \
                 directory",
                file.path
            )));
        }
        Ok(layout)
    }

    /// Packs the layout for upload. Header ids are the container's own (podman maps them);
    /// `mtime` is the files' modification time, in seconds since the epoch.
    pub fn archives(&self, mtime: u64) -> io::Result<Archives> {
        let mut ids = Archive::new(mtime);
        let mut container_user = Archive::new(mtime);
        for dir in &self.dirs {
            ids.directory(dir)?;
        }
        for (path, file) in &self.files {
            match file.owner {
                Ownership::Ids(owner) => ids.file(path, file, owner)?,
                // The header's owner is replaced by `copyUIDGID`.
                Ownership::ContainerUser => container_user.file(path, file, FileOwner::ROOT)?,
            }
        }
        Ok(Archives {
            ids: ids.finish()?,
            container_user: container_user.finish()?,
        })
    }
}

/// A tar archive being written, with paths relative to `/`.
struct Archive {
    builder: tar::Builder<Vec<u8>>,
    mtime: u64,
    entries: usize,
}

impl Archive {
    /// What missing parent directories are created as.
    const DIR_MODE: u32 = 0o755;

    fn new(mtime: u64) -> Self {
        Self {
            builder: tar::Builder::new(Vec::new()),
            mtime,
            entries: 0,
        }
    }

    fn header(&self, kind: tar::EntryType, mode: u32, owner: FileOwner, size: u64) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_uid(u64::from(owner.uid));
        header.set_gid(u64::from(owner.gid));
        header.set_mtime(self.mtime);
        header.set_size(size);
        header
    }

    fn directory(&mut self, path: &str) -> io::Result<()> {
        let mut header = self.header(
            tar::EntryType::Directory,
            Self::DIR_MODE,
            FileOwner::ROOT,
            0,
        );
        self.entries += 1;
        self.builder
            .append_data(&mut header, format!("{}/", &path[1..]), io::empty())
    }

    fn file(&mut self, path: &str, file: &ContainerFile, owner: FileOwner) -> io::Result<()> {
        let content = file.content.expose().as_bytes();
        let mut header = self.header(
            tar::EntryType::Regular,
            u32::from(file.mode),
            owner,
            content.len() as u64,
        );
        self.entries += 1;
        self.builder.append_data(&mut header, &path[1..], content)
    }

    /// The archive's bytes, or `None` when nothing was added.
    fn finish(self) -> io::Result<Option<Vec<u8>>> {
        let entries = self.entries;
        let bytes = self.builder.into_inner()?;
        Ok((entries > 0).then_some(bytes))
    }
}

/// podman's answer to a failed upload, reduced to what a deployment may show (ADR-0057 §3).
///
/// The message can be a multi-KB dump of the request that names host storage paths. Only the
/// text after the last `copier: put:` is the reason, and names container paths alone:
/// `error setting ownership of "/etc/app/b.conf" to 70000:0: lchown /etc/app/b.conf: invalid
/// argument`. Without that marker nothing of the message is shown.
pub(super) struct UploadFailure<'a>(pub &'a str);

impl UploadFailure<'_> {
    const MARKER: &'static str = "copier: put:";
    /// The longest reason shown; podman's are a line.
    const MAX_CHARS: usize = 300;

    pub fn reason(&self) -> String {
        let tail = self
            .0
            .rsplit_once(Self::MARKER)
            .map(|(_, tail)| tail.lines().next().unwrap_or_default().trim())
            .filter(|tail| !tail.is_empty());
        match tail {
            Some(tail) => tail.chars().take(Self::MAX_CHARS).collect(),
            None => "podman refused the upload".to_owned(),
        }
    }
}

impl From<UploadFailure<'_>> for RuntimeError {
    fn from(failure: UploadFailure<'_>) -> Self {
        RuntimeError::ConfigFiles {
            message: failure.reason(),
        }
    }
}

#[cfg(test)]
mod tests;
