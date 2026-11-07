//! Crash-safe file writes for the ACME state on disk (ADR-0016): a temporary sibling file is
//! written, fsynced and renamed over the target, then the directory is fsynced, so a reader sees
//! either the old contents or the new ones, never a torn file.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// A file replaced atomically on every write.
#[derive(Debug, Clone, Copy)]
pub(super) struct AtomicFile<'a> {
    path: &'a Path,
    mode: u32,
}

impl<'a> AtomicFile<'a> {
    /// Key material and credentials: readable by hoster only.
    pub(super) const PRIVATE: u32 = 0o600;
    /// Public material such as certificate chains.
    pub(super) const PUBLIC: u32 = 0o644;

    /// The file at `path`, written with permission bits `mode`.
    pub(super) fn new(path: &'a Path, mode: u32) -> Self {
        Self { path, mode }
    }

    /// The temporary sibling the contents are staged in: `<name>.tmp`.
    pub(super) fn tmp_path(&self) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(OsString::from)
            .unwrap_or_default();
        name.push(".tmp");
        self.path.with_file_name(name)
    }

    /// Creates the parent directory (mode 0700) if needed and replaces the file with `contents`.
    pub(super) fn write(&self, contents: &[u8]) -> io::Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("path has no parent directory"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;

        let tmp = self.tmp_path();
        // A leftover from a crash keeps its old mode; `mode` only applies to a new file.
        let _ = fs::remove_file(&tmp);
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(self.mode)
                .open(&tmp)?;
            file.write_all(contents)?;
            file.sync_all()?;
            fs::rename(&tmp, self.path)?;
            fs::File::open(dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}
