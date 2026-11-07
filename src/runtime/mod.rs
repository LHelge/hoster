//! The `ContainerRuntime` trait, its bollard-backed rootless podman implementation (ADR-0002) and a
//! fake for tests. Integration tests against a real podman are gated by `HOSTER_PODMAN_TESTS=1`.
//!
//! The trait speaks hoster's own types ([`ContainerSpec`], [`ContainerInfo`], [`ContainerState`],
//! …); bollard stays an implementation detail of [`PodmanRuntime`]. Object naming and labels
//! follow docs/architecture.md §8.

#[cfg(test)]
pub mod fake;
mod podman;

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use futures_util::stream::BoxStream;

use crate::stack::ImageRef;

#[allow(
    unused_imports,
    reason = "the fake is used by the orchestrator's tests"
)]
#[cfg(test)]
pub use fake::FakeRuntime;
pub use podman::{PodmanRuntime, PodmanSocket};

/// Podman object labels (docs/architecture.md §8).
#[allow(
    dead_code,
    reason = "consumed by the deploy engine as it is implemented"
)]
pub mod label {
    /// `true` on every object hoster created; the reconciler lists by it.
    pub const MANAGED: &str = "hoster.managed";
    pub const STACK: &str = "hoster.stack";
    pub const SERVICE: &str = "hoster.service";
    pub const GENERATION: &str = "hoster.generation";
    /// The image repository, without tag or digest.
    pub const IMAGE: &str = "hoster.image";
    /// The *requested* digest (what hoster asked for, not what podman reports).
    pub const DIGEST: &str = "hoster.digest";
    pub const DEPLOYMENT: &str = "hoster.deployment";
}

/// Object labels, ordered so specs compare and print deterministically.
pub type Labels = BTreeMap<String, String>;

/// The container runtime the deploy engine drives (ADR-0002).
///
/// Every method is idempotent where the description says so, and reports failures as
/// [`RuntimeError`]. Futures and streams are `Send` so they can run on spawned tasks.
#[allow(
    dead_code,
    reason = "consumed by the deploy engine as it is implemented"
)]
pub trait ContainerRuntime: Send + Sync + 'static {
    /// Creates the network unless one with that name exists (idempotent).
    fn ensure_network(
        &self,
        network: &NetworkSpec,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Removes a network. Fails with [`RuntimeError::InUse`] while containers are attached.
    fn remove_network(&self, name: &str) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Makes `image` available locally, pulling it if it is not present.
    fn pull_image(&self, image: &ImageRef)
    -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Creates (but does not start) a container. Fails with [`RuntimeError::Conflict`] when the
    /// name is taken, e.g. by a leftover of an interrupted deploy.
    fn create_container(
        &self,
        spec: &ContainerSpec,
    ) -> impl Future<Output = Result<ContainerId, RuntimeError>> + Send;

    /// Starts a created or stopped container; starting a running one is a no-op.
    fn start(&self, id: &ContainerId) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Stops a container, killing it after `timeout`; stopping a stopped one is a no-op.
    fn stop(
        &self,
        id: &ContainerId,
        timeout: Duration,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Removes a container, killing it first if it is still running.
    fn remove(&self, id: &ContainerId) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// The current state of one container.
    fn inspect(
        &self,
        id: &ContainerId,
    ) -> impl Future<Output = Result<ContainerInfo, RuntimeError>> + Send;

    /// Every container labelled `hoster.managed=true`, running or not.
    fn list_managed(&self)
    -> impl Future<Output = Result<Vec<ContainerInfo>, RuntimeError>> + Send;

    /// A container's log, optionally followed live (ADR-0044).
    fn logs(
        &self,
        id: &ContainerId,
        options: LogOptions,
    ) -> BoxStream<'static, Result<LogLine, RuntimeError>>;
}

/// A podman container ID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContainerId(String);

impl From<String> for ContainerId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl AsRef<str> for ContainerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stack network (`hoster-<stack>`, ADR-0006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSpec {
    pub name: String,
    pub labels: Labels,
}

/// Everything needed to create one container generation of a service.
///
/// There is deliberately no way to express privileged mode, host namespaces, devices, extra
/// capabilities or bind mounts (ADR-0019), and ports are only ever published on 127.0.0.1
/// (ADR-0007).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "consumed by the deploy engine as it is implemented"
)]
pub struct ContainerSpec {
    /// `hoster-<stack>-<service>-g<generation>`.
    pub name: String,
    pub image: ImageRef,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub labels: Labels,
    /// The stack network to join, and the alias (the service name) to join it with.
    pub network: String,
    pub network_alias: String,
    pub ports: Vec<PortBinding>,
    pub volumes: Vec<VolumeMount>,
    pub user: Option<String>,
    pub read_only: bool,
    pub stop_timeout: Duration,
}

/// A container port published on `127.0.0.1:<host_port>` (ADR-0007, ADR-0026).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortBinding {
    pub container_port: u16,
    pub host_port: u16,
}

/// A named podman volume (`hoster-<stack>-<volume>`, ADR-0030) mounted at `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "consumed by the deploy engine as it is implemented"
)]
pub struct VolumeMount {
    pub volume: String,
    pub target: String,
}

/// What the runtime reports about one container.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code, reason = "consumed by the reconciler as it is implemented")]
pub struct ContainerInfo {
    pub id: ContainerId,
    pub name: String,
    pub labels: Labels,
    pub state: ContainerState,
    /// The published ports, as podman reports them.
    pub ports: Vec<PortBinding>,
}

impl ContainerInfo {
    /// Whether the container carries `hoster.managed=true`.
    #[allow(dead_code, reason = "consumed by the reconciler")]
    pub fn is_managed(&self) -> bool {
        self.labels.get(label::MANAGED).is_some_and(|v| v == "true")
    }
}

/// A container's lifecycle state, as podman's compat API names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerState {
    Created,
    Running,
    Paused,
    Restarting,
    Removing,
    /// Stopped, with the exit code if known.
    Exited(Option<i64>),
    Dead,
}

impl ContainerState {
    /// Whether the container's process is up (running, paused or restarting).
    #[allow(dead_code, reason = "consumed by the reconciler")]
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Paused | Self::Restarting)
    }
}

impl FromStr for ContainerState {
    type Err = String;

    /// Parses the compat API's `State.Status` (podman also reports `configured` and `stopped`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "created" | "configured" | "initialized" => Self::Created,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "restarting" => Self::Restarting,
            "removing" => Self::Removing,
            "exited" | "stopped" | "stopping" => Self::Exited(None),
            "dead" => Self::Dead,
            other => return Err(format!("unknown container state `{other}`")),
        })
    }
}

impl fmt::Display for ContainerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Created => f.write_str("created"),
            Self::Running => f.write_str("running"),
            Self::Paused => f.write_str("paused"),
            Self::Restarting => f.write_str("restarting"),
            Self::Removing => f.write_str("removing"),
            Self::Exited(Some(code)) => write!(f, "exited ({code})"),
            Self::Exited(None) => f.write_str("exited"),
            Self::Dead => f.write_str("dead"),
        }
    }
}

/// Which log lines to return.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogOptions {
    /// Keep the stream open and yield new lines as they are written.
    pub follow: bool,
    /// Only the last `n` lines of the existing log; all of it when `None`.
    pub tail: Option<usize>,
}

/// One line of container output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub stream: LogStream,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "constructed by the podman log stream")]
pub enum LogStream {
    Stdout,
    Stderr,
}

/// A [`ContainerRuntime`] call, for error messages and failure injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(dead_code, reason = "named by the fake runtime and the podman stubs")]
pub enum Operation {
    Connect,
    EnsureNetwork,
    RemoveNetwork,
    PullImage,
    CreateContainer,
    Start,
    Stop,
    Remove,
    Inspect,
    ListManaged,
    Logs,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Connect => "connect",
            Self::EnsureNetwork => "ensure network",
            Self::RemoveNetwork => "remove network",
            Self::PullImage => "pull image",
            Self::CreateContainer => "create container",
            Self::Start => "start container",
            Self::Stop => "stop container",
            Self::Remove => "remove container",
            Self::Inspect => "inspect container",
            Self::ListManaged => "list containers",
            Self::Logs => "read logs",
        })
    }
}

/// The kind of podman object an error is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "named by the fake runtime and podman operations")]
pub enum ObjectKind {
    Network,
    Container,
    Image,
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Network => "network",
            Self::Container => "container",
            Self::Image => "image",
        })
    }
}

/// Why a runtime call failed. Carries messages, not bollard errors, so it can be cloned (for
/// failure injection) and compared in tests.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[allow(
    dead_code,
    reason = "variants are raised as the operations are implemented"
)]
pub enum RuntimeError {
    #[error(
        "no podman socket at {path}; is the hoster user's podman.socket unit enabled and \
         running? Run `hoster doctor` to check the host setup"
    )]
    SocketMissing {
        /// The path, or the discovery candidates as "`a` or `b`".
        path: String,
    },
    #[error(
        "cannot reach podman at {socket}: {message}; run `hoster doctor` to check the host setup"
    )]
    Unreachable { socket: String, message: String },
    #[error("{kind} {name} not found")]
    NotFound { kind: ObjectKind, name: String },
    #[error("{kind} {name} already exists")]
    Conflict { kind: ObjectKind, name: String },
    #[error("{kind} {name} is in use")]
    InUse { kind: ObjectKind, name: String },
    #[error("{operation} is not implemented yet")]
    Unsupported { operation: Operation },
    #[error("podman failed to {operation}: {message}")]
    Api {
        operation: Operation,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_state_parses_compat_and_podman_names() {
        assert_eq!("running".parse(), Ok(ContainerState::Running));
        assert_eq!("Exited".parse(), Ok(ContainerState::Exited(None)));
        assert_eq!("configured".parse(), Ok(ContainerState::Created));
        assert_eq!("stopped".parse(), Ok(ContainerState::Exited(None)));
        assert!("bogus".parse::<ContainerState>().is_err());
    }

    #[test]
    fn container_state_display() {
        assert_eq!(
            ContainerState::Exited(Some(137)).to_string(),
            "exited (137)"
        );
        assert_eq!(ContainerState::Running.to_string(), "running");
        assert!(ContainerState::Paused.is_running());
        assert!(!ContainerState::Exited(Some(0)).is_running());
    }

    #[test]
    fn managed_label_must_be_true() {
        let mut info = ContainerInfo {
            id: ContainerId::from("abc".to_owned()),
            name: "hoster-notes-web-g1".into(),
            labels: Labels::new(),
            state: ContainerState::Created,
            ports: Vec::new(),
        };
        assert!(!info.is_managed());
        info.labels.insert(label::MANAGED.into(), "false".into());
        assert!(!info.is_managed());
        info.labels.insert(label::MANAGED.into(), "true".into());
        assert!(info.is_managed());
    }

    #[test]
    fn socket_missing_points_at_doctor() {
        let err = RuntimeError::SocketMissing {
            path: "/run/user/1000/podman/podman.sock".into(),
        };
        let message = err.to_string();
        assert!(message.contains("/run/user/1000/podman/podman.sock"));
        assert!(message.contains("hoster doctor"));
    }
}
