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
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Component, Path};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use futures_util::stream::BoxStream;

use crate::registry::CredentialError;
use crate::stack::{DEFAULT_STOP_TIMEOUT, Name, PinnedImage};
use crate::store::DeploymentId;

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

    /// Makes `image` available locally, pulling it by digest (with the registry's credential,
    /// if one is configured) unless it is already present. Returns once the pull is complete.
    ///
    /// Fails with [`RuntimeError::NotFound`] (kind [`ObjectKind::Image`]) when the registry has
    /// no such digest, and [`RuntimeError::RegistryAuth`] when it refuses access.
    fn pull_image(
        &self,
        image: &PinnedImage,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send;

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

/// The prefix of every podman object hoster names (docs/architecture.md §8).
const PREFIX: &str = "hoster";

/// A stack network (`hoster-<stack>`, ADR-0006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSpec {
    pub name: String,
    pub labels: Labels,
}

#[allow(
    dead_code,
    reason = "consumed by the deploy engine as it is implemented"
)]
impl NetworkSpec {
    /// `hoster-<stack>`, labelled `hoster.managed=true` and `hoster.stack=<stack>`.
    pub fn for_stack(stack: &Name) -> Self {
        Self {
            name: format!("{PREFIX}-{stack}"),
            labels: Labels::from([
                (label::MANAGED.into(), "true".into()),
                (label::STACK.into(), stack.to_string()),
            ]),
        }
    }
}

/// One generation of one service of a stack: what a container *is*. Its `Display` is the
/// container name, `hoster-<stack>-<service>-g<generation>` (docs/architecture.md §8).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[allow(
    dead_code,
    reason = "consumed by the deploy engine as it is implemented"
)]
pub struct ServiceGeneration {
    pub stack: Name,
    pub service: Name,
    pub generation: u32,
}

impl fmt::Display for ServiceGeneration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{PREFIX}-{}-{}-g{}",
            self.stack, self.service, self.generation
        )
    }
}

/// Everything needed to create one container generation of a service.
///
/// There is deliberately no way to express privileged mode, host namespaces, devices, extra
/// capabilities or bind mounts (ADR-0019): [`PodmanRuntime`] pins each of those to its safe value
/// itself, and the only mounts are named volumes ([`VolumeMount`]). Ports are only ever published
/// on 127.0.0.1 (ADR-0007), and podman's restart policy is always `no` (ADR-0028).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "consumed by the deploy engine as it is implemented"
)]
pub struct ContainerSpec {
    /// `hoster-<stack>-<service>-g<generation>`.
    pub name: String,
    pub image: PinnedImage,
    pub command: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub env: BTreeMap<String, String>,
    pub labels: Labels,
    /// The stack network to join, and the alias (the service name) to join it with. Both are
    /// derived from the [`ServiceGeneration`] by [`ContainerSpec::new`] and cannot be set from
    /// outside the runtime: a free-form network would admit `host` or `container:<id>`
    /// (ADR-0019) or another stack's network (ADR-0006).
    network: String,
    network_alias: String,
    pub ports: Vec<PortBinding>,
    pub volumes: Vec<VolumeMount>,
    pub user: Option<String>,
    pub read_only: bool,
    pub stop_timeout: Duration,
}

#[allow(
    dead_code,
    reason = "consumed by the deploy engine as it is implemented"
)]
impl ContainerSpec {
    /// A spec for `generation` running `image`, created by `deployment`: the name, the full
    /// label set, and the stack network joined with the service name as alias. Everything else
    /// (command, env, ports, volumes, …) starts empty or at its default.
    ///
    /// `image` is pinned by digest; that digest is recorded as `hoster.digest`, the *requested*
    /// digest (podman reports platform-specific image IDs instead).
    pub fn new(
        generation: &ServiceGeneration,
        deployment: DeploymentId,
        image: PinnedImage,
    ) -> Self {
        let labels = Labels::from([
            (label::MANAGED.into(), "true".into()),
            (label::STACK.into(), generation.stack.to_string()),
            (label::SERVICE.into(), generation.service.to_string()),
            (label::GENERATION.into(), generation.generation.to_string()),
            (
                label::IMAGE.into(),
                format!("{}/{}", image.registry(), image.repository()),
            ),
            (label::DIGEST.into(), image.digest().to_string()),
            (label::DEPLOYMENT.into(), deployment.to_string()),
        ]);
        Self {
            name: generation.to_string(),
            image,
            command: None,
            entrypoint: None,
            env: BTreeMap::new(),
            labels,
            network: NetworkSpec::for_stack(&generation.stack).name,
            network_alias: generation.service.to_string(),
            ports: Vec::new(),
            volumes: Vec::new(),
            user: None,
            read_only: false,
            stop_timeout: DEFAULT_STOP_TIMEOUT,
        }
    }

    /// The stack network the container joins, `hoster-<stack>`.
    pub fn network(&self) -> &str {
        &self.network
    }

    /// The alias the container joins its network with: the service name.
    pub fn network_alias(&self) -> &str {
        &self.network_alias
    }
}

/// A container port published on `127.0.0.1:<host_port>` (ADR-0007, ADR-0026). There is no host
/// IP to choose: loopback is the only one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortBinding {
    pub container_port: u16,
    pub host_port: u16,
}

impl PortBinding {
    /// The only address hoster ever publishes on.
    pub const HOST_IP: Ipv4Addr = Ipv4Addr::LOCALHOST;
}

/// A named podman volume (`hoster-<stack>-<volume>`, ADR-0030) mounted at `target`.
///
/// Built only through [`VolumeMount::named`], so it can never be a bind mount of a host path
/// (ADR-0019).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    volume: String,
    target: String,
}

#[allow(
    dead_code,
    reason = "consumed by the deploy engine as it is implemented"
)]
impl VolumeMount {
    /// The stack's volume `volume` (podman name `hoster-<stack>-<volume>`) mounted at the
    /// absolute path `target`.
    /// The volume name follows the stack format's rules for names, and the target must be an
    /// absolute path below `/` without `.` or `..` segments or `:`.
    pub fn named(stack: &Name, volume: &str, target: &str) -> Result<Self, String> {
        let volume: Name = volume.parse()?;
        let path = Path::new(target);
        let mut components = path.components();
        let below_root = components.next() == Some(Component::RootDir)
            && target.split('/').skip(1).all(|s| !matches!(s, "." | ".."))
            && components.all(|c| matches!(c, Component::Normal(_)))
            && path.components().count() > 1;
        if !below_root || target.contains(':') {
            return Err(format!(
                "invalid mount path `{target}`: must be an absolute path below `/` without `.`, \
                 `..` or `:`"
            ));
        }
        Ok(Self {
            volume: format!("{PREFIX}-{stack}-{volume}"),
            target: target.to_owned(),
        })
    }

    /// The podman volume name, `hoster-<stack>-<volume>`.
    pub fn volume(&self) -> &str {
        &self.volume
    }

    /// The absolute mount path inside the container.
    pub fn target(&self) -> &str {
        &self.target
    }
}

/// A published port as podman reports it, host IP included, so a binding that is not on loopback
/// shows up instead of being assumed away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublishedPort {
    pub container_port: u16,
    pub host_ip: IpAddr,
    pub host_port: u16,
}

impl PublishedPort {
    /// Whether this is a binding hoster could have made: on 127.0.0.1.
    #[allow(dead_code, reason = "consumed by the reconciler")]
    pub fn is_loopback(&self) -> bool {
        self.host_ip == IpAddr::V4(PortBinding::HOST_IP)
    }
}

impl From<PortBinding> for PublishedPort {
    fn from(binding: PortBinding) -> Self {
        Self {
            container_port: binding.container_port,
            host_ip: PortBinding::HOST_IP.into(),
            host_port: binding.host_port,
        }
    }
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
    pub ports: Vec<PublishedPort>,
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
    /// Only lines written at or after this time. Podman resolves it to whole seconds, so a
    /// caller that needs more precision filters on [`LogLine::time`] as well.
    pub since: Option<SystemTime>,
}

/// One line of container output, without its line break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub stream: LogStream,
    /// When podman recorded the line, if it said.
    pub time: Option<SystemTime>,
    pub text: String,
}

/// Which output stream a [`LogLine`] was written to. Its `Display` is the lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl fmt::Display for LogStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
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
    /// The registry refused the pull: missing, wrong or insufficient credentials. The message
    /// is the registry's; credentials are never part of it.
    #[error(
        "{registry} refused to serve {image}: {message}; check the registry credential for \
         {registry}"
    )]
    RegistryAuth {
        registry: String,
        image: String,
        message: String,
    },
    /// The configured credential for a registry could not be read.
    #[error("cannot use the registry credential: {message}")]
    Credentials { registry: String, message: String },
    #[error("{operation} is not implemented yet")]
    Unsupported { operation: Operation },
    #[error("podman failed to {operation}: {message}")]
    Api {
        operation: Operation,
        message: String,
    },
}

impl From<CredentialError> for RuntimeError {
    fn from(err: CredentialError) -> Self {
        Self::Credentials {
            registry: err.registry().to_owned(),
            message: err.to_string(),
        }
    }
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

    fn name(s: &str) -> Name {
        s.parse().unwrap()
    }

    #[test]
    fn network_is_named_and_labelled_per_stack() {
        let network = NetworkSpec::for_stack(&name("notes"));
        assert_eq!(network.name, "hoster-notes");
        assert_eq!(
            network.labels,
            Labels::from([
                ("hoster.managed".into(), "true".into()),
                ("hoster.stack".into(), "notes".into()),
            ])
        );
    }

    #[test]
    fn container_spec_carries_name_labels_and_network() {
        let generation = ServiceGeneration {
            stack: name("notes"),
            service: name("web"),
            generation: 7,
        };
        let digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let image: PinnedImage = format!("ghcr.io/me/notes@{digest}").parse().unwrap();
        let spec = ContainerSpec::new(&generation, DeploymentId::from(42), image.clone());
        assert_eq!(spec.name, "hoster-notes-web-g7");
        assert_eq!(spec.network(), "hoster-notes");
        assert_eq!(spec.network_alias(), "web");
        assert_eq!(spec.image, image);
        assert_eq!(spec.stop_timeout, DEFAULT_STOP_TIMEOUT);
        assert_eq!(
            spec.labels,
            Labels::from([
                ("hoster.managed".into(), "true".into()),
                ("hoster.stack".into(), "notes".into()),
                ("hoster.service".into(), "web".into()),
                ("hoster.generation".into(), "7".into()),
                ("hoster.image".into(), "ghcr.io/me/notes".into()),
                ("hoster.digest".into(), digest.into()),
                ("hoster.deployment".into(), "42".into()),
            ])
        );
        // A tag carried alongside the digest stays out of the image label.
        let tagged = ContainerSpec::new(
            &generation,
            DeploymentId::from(1),
            format!("docker.io/library/busybox:1.37@{digest}")
                .parse()
                .unwrap(),
        );
        assert_eq!(tagged.labels[label::DIGEST], digest);
        assert_eq!(tagged.labels[label::IMAGE], "docker.io/library/busybox");
    }

    #[test]
    fn volume_mounts_are_named_volumes_only() {
        let stack = name("notes");
        let mount = VolumeMount::named(&stack, "data", "/var/lib/notes").unwrap();
        assert_eq!(mount.volume(), "hoster-notes-data");
        assert_eq!(mount.target(), "/var/lib/notes");
        for volume in ["", "/etc", "../x", "a/b", "-x", "Data", "a_b"] {
            assert!(
                VolumeMount::named(&stack, volume, "/data").is_err(),
                "{volume}"
            );
        }
        for target in ["data", "/data/../etc", "/a:/b", "./x", "", "/", "/a/./b"] {
            assert!(
                VolumeMount::named(&stack, "data", target).is_err(),
                "{target}"
            );
        }
    }

    #[test]
    fn published_ports_from_bindings_are_loopback() {
        let port = PublishedPort::from(PortBinding {
            container_port: 80,
            host_port: 20001,
        });
        assert!(port.is_loopback());
        let wide = PublishedPort {
            host_ip: Ipv4Addr::UNSPECIFIED.into(),
            ..port
        };
        assert!(!wide.is_loopback());
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
