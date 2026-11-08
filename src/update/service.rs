//! Starting and stopping the hoster service (`systemctl --user`, ADR-0023).

use std::ffi::OsStr;
use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use async_trait::async_trait;
use thiserror::Error;

/// What is done to the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

impl fmt::Display for ServiceAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ServiceAction::Start => "start",
            ServiceAction::Stop => "stop",
            ServiceAction::Restart => "restart",
        })
    }
}

/// A service action that failed.
#[derive(Debug, Error)]
#[error(
    "cannot {action} the hoster service: {detail}; check `systemctl --user status hoster` and \
     `journalctl --user -u hoster`"
)]
pub struct ServiceError {
    pub action: ServiceAction,
    pub detail: String,
}

/// Controls the hoster service. Production uses [`SystemdUserService`]; tests a fake.
#[async_trait]
pub trait ServiceControl: Send + Sync {
    /// Performs `action`, returning once systemd has done it.
    async fn perform(&self, action: ServiceAction) -> Result<(), ServiceError>;
}

/// The `hoster.service` user unit, driven with `systemctl --user`.
#[derive(Debug, Clone, Default)]
pub struct SystemdUserService {
    /// `XDG_RUNTIME_DIR` for `systemctl` when this process has none (e.g. under
    /// `sudo -iu hoster`), so it still finds the user manager.
    runtime_dir: Option<PathBuf>,
}

impl SystemdUserService {
    const UNIT: &str = "hoster.service";

    /// The service as this process sees it: with `/run/user/<uid>` as the runtime directory
    /// when `XDG_RUNTIME_DIR` is unset.
    pub fn from_env() -> Self {
        // The owner of /proc/self is this process's (effective) uid; no libc needed.
        let uid = std::fs::metadata("/proc/self").ok().map(|m| m.uid());
        let xdg = std::env::var_os("XDG_RUNTIME_DIR");
        Self {
            runtime_dir: uid.and_then(|uid| Self::fallback_runtime_dir(xdg.as_deref(), uid)),
        }
    }

    /// `/run/user/<uid>` when `xdg` (the process's `XDG_RUNTIME_DIR`) is unset or empty.
    pub(super) fn fallback_runtime_dir(xdg: Option<&OsStr>, uid: u32) -> Option<PathBuf> {
        xdg.is_none_or(OsStr::is_empty)
            .then(|| PathBuf::from(format!("/run/user/{uid}")))
    }
}

#[async_trait]
impl ServiceControl for SystemdUserService {
    async fn perform(&self, action: ServiceAction) -> Result<(), ServiceError> {
        let mut command = tokio::process::Command::new("systemctl");
        command
            .args(["--user", &action.to_string(), Self::UNIT])
            .stdin(std::process::Stdio::null());
        if let Some(dir) = &self.runtime_dir {
            command.env("XDG_RUNTIME_DIR", dir);
        }
        let output = command.output().await.map_err(|e| ServiceError {
            action,
            detail: format!("cannot run systemctl: {e}"),
        })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(ServiceError {
                action,
                detail: format!(
                    "systemctl {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            })
        }
    }
}
