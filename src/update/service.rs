//! Starting and stopping the hoster service (`systemctl --user`, ADR-0023).

use std::fmt;

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
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemdUserService;

impl SystemdUserService {
    const UNIT: &str = "hoster.service";
}

#[async_trait]
impl ServiceControl for SystemdUserService {
    async fn perform(&self, action: ServiceAction) -> Result<(), ServiceError> {
        let output = tokio::process::Command::new("systemctl")
            .args(["--user", &action.to_string(), Self::UNIT])
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| ServiceError {
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
