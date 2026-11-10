//! [`PodmanRuntime`]: the rootless podman socket through bollard's Docker-compatible client
//! (ADR-0002, docs/architecture.md §1.3).

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::ops::ControlFlow;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bollard::auth::DockerCredentials;
use bollard::container::LogOutput;
use bollard::errors::Error as BollardError;
use bollard::exec::{StartExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, ContainerInspectResponse, ContainerSummary, EndpointSettings, ExecConfig,
    ExecInspectResponse, HealthConfig, HostConfig, HostConfigLogConfig, Mount, MountType,
    NetworkCreateRequest, NetworkingConfig, PortMap, PortSummaryTypeEnum, RestartPolicy,
    RestartPolicyNameEnum, VolumeCreateRequest,
};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, CreateImageOptionsBuilder, ListContainersOptionsBuilder,
    ListNetworksOptionsBuilder, LogsOptions, LogsOptionsBuilder, RemoveContainerOptionsBuilder,
    RemoveImageOptionsBuilder, RemoveVolumeOptions, StopContainerOptionsBuilder,
    UploadToContainerOptionsBuilder,
};
use bollard::{API_DEFAULT_VERSION, Docker, body_full};
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::exec::{Argv, ExecProcess, OutputTail};
use super::files::{Layout, PathStat, StatPath, UploadFailure};
use super::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerSpec, ContainerState, EngineInfo,
    ExecGuard, ExecOutcome, ExecOutput, LogLine, LogOptions, LogStream, NetworkSpec, ObjectKind,
    Operation, PortBinding, PublishedPort, RunningExec, RuntimeError, VolumeSpec, label,
};
use crate::registry::{Anonymous, RegistryCredential, RegistryCredentials};
use crate::stack::PinnedImage;

/// Where the rootless podman API socket is looked for: the configured path, or else the hoster
/// user's own `podman.socket` (ADR-0002, ADR-0023). `hoster doctor` checks the same paths.
///
/// Discovery follows the rootless steps of bollard's `connect_with_podman_defaults()`
/// (docs/architecture.md §1.3) but deliberately stops there: bollard would go on to the rootful
/// `/run/podman/podman.sock` and `/var/run/docker.sock`, which ADR-0002 rules out. `$DOCKER_HOST`
/// is not consulted either, as it may name a rootful engine; `podman_socket` in hoster.toml is
/// the explicit override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanSocket {
    /// Paths to try, in order; never empty.
    candidates: Vec<PathBuf>,
}

impl PodmanSocket {
    /// Exactly `path`, with no discovery.
    pub fn configured(path: &Path) -> Self {
        Self {
            candidates: vec![path.to_path_buf()],
        }
    }

    /// `$XDG_RUNTIME_DIR/podman/podman.sock`, then `/run/user/$UID/podman/podman.sock`, without
    /// duplicates. The second is left out when the uid is unknown.
    pub fn discover(runtime_dir: Option<&Path>, uid: Option<u32>) -> Self {
        let fallback = uid.map(|uid| PathBuf::from(format!("/run/user/{uid}")));
        let mut candidates: Vec<PathBuf> = runtime_dir
            .into_iter()
            .chain(fallback.as_deref())
            .map(|dir| dir.join("podman").join("podman.sock"))
            .collect();
        candidates.dedup();
        if candidates.is_empty() {
            // Neither is known; name the canonical location so messages still point somewhere.
            candidates.push(PathBuf::from("$XDG_RUNTIME_DIR/podman/podman.sock"));
        }
        Self { candidates }
    }

    /// `configured` if set, otherwise discovery from this process's `$XDG_RUNTIME_DIR` and uid.
    pub fn from_env(configured: Option<&Path>) -> Self {
        match configured {
            Some(path) => Self::configured(path),
            None => {
                let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from);
                // The owner of /proc/self is this process's (effective) uid; no libc needed.
                let uid = std::fs::metadata("/proc/self").ok().map(|m| m.uid());
                Self::discover(runtime_dir.as_deref(), uid)
            }
        }
    }

    /// The paths tried, in order.
    pub fn candidates(&self) -> &[PathBuf] {
        &self.candidates
    }

    /// The first candidate that exists.
    pub fn find(&self) -> Option<&Path> {
        self.candidates
            .iter()
            .map(PathBuf::as_path)
            .find(|path| path.exists())
    }
}

/// The candidates, as "`a` or `b`".
impl fmt::Display for PodmanSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, path) in self.candidates.iter().enumerate() {
            if i > 0 {
                f.write_str(" or ")?;
            }
            write!(f, "{}", path.display())?;
        }
        Ok(())
    }
}

/// A connected rootless podman.
#[derive(Debug, Clone)]
#[allow(
    dead_code,
    reason = "`socket` is reported by the operations implemented later"
)]
pub struct PodmanRuntime {
    docker: Docker,
    /// The socket connected to.
    socket: PathBuf,
    /// podman's version as it reported it at connect; `unknown` when it did not.
    version: String,
    /// Pull credentials per registry host (ADR-0025); anonymous unless set.
    credentials: Arc<dyn RegistryCredentials>,
    /// Finished exec sessions still to be removed.
    sessions: Arc<ExecSessions>,
}

impl PodmanRuntime {
    /// Read/write timeout for every request to the socket, in seconds. Long-running calls
    /// (pulls, followed logs) are streams and are not cut off by it.
    const TIMEOUT_SECS: u64 = 120;

    /// Connects to the first existing socket of `socket` (see [`PodmanSocket`]), then
    /// negotiates the API version and pings (startup step 4, docs/architecture.md §4).
    ///
    /// bollard defaults to API 1.53 while podman 6.1 serves 1.44, so the version must be
    /// negotiated down before any other call (§1.3).
    pub async fn connect(socket: &PodmanSocket) -> Result<Self, RuntimeError> {
        let missing = |path: String| RuntimeError::SocketMissing { path };
        let path = socket
            .find()
            .ok_or_else(|| missing(socket.to_string()))?
            .to_path_buf();
        let shown = path.display().to_string();
        let docker = Docker::connect_with_unix(&shown, Self::TIMEOUT_SECS, API_DEFAULT_VERSION)
            .map_err(|err| match err {
                // Gone between the existence check and the connect.
                BollardError::SocketNotFoundError(path) => missing(path),
                other => RuntimeError::Unreachable {
                    socket: shown.clone(),
                    message: other.to_string(),
                },
            })?;
        let unreachable = |err: BollardError| RuntimeError::Unreachable {
            socket: shown.clone(),
            message: err.to_string(),
        };
        let docker = docker.negotiate_version().await.map_err(unreachable)?;
        docker.ping().await.map_err(unreachable)?;
        let version = docker.version().await.map_err(unreachable)?;

        // podman's compat /version reports itself as the "Podman Engine" component.
        let podman_version = version
            .components
            .iter()
            .flatten()
            .find(|c| c.name.to_ascii_lowercase().contains("podman"))
            .map(|c| c.version.clone())
            .or(version.version)
            .unwrap_or_else(|| "unknown".to_owned());
        tracing::info!(
            socket = %shown,
            api_version = %docker.client_version(),
            server_api_version = version.api_version.as_deref().unwrap_or("unknown"),
            podman_version = %podman_version,
            "connected to podman"
        );
        Ok(Self {
            docker,
            socket: path,
            version: podman_version,
            credentials: Arc::new(Anonymous),
            sessions: Arc::default(),
        })
    }

    /// Uses `credentials` for image pulls instead of pulling anonymously.
    pub fn with_credentials(mut self, credentials: Arc<dyn RegistryCredentials>) -> Self {
        self.credentials = credentials;
        self
    }

    /// Whether the image is in local storage already.
    async fn has_image(&self, reference: &str) -> Result<bool, RuntimeError> {
        match self.docker.inspect_image(reference).await {
            Ok(_) => Ok(true),
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(err) => Err(RuntimeError::Api {
                operation: Operation::PullImage,
                message: format!("cannot inspect image {reference}: {err}"),
            }),
        }
    }

    /// The client to stop a container with: the shared one, unless podman may legitimately take
    /// longer than [`Self::TIMEOUT_SECS`] to answer because the container gets `timeout` to exit.
    fn client_for_stop(&self, timeout: Duration) -> Docker {
        let needed = timeout + Duration::from_secs(30);
        if needed.as_secs() > Self::TIMEOUT_SECS {
            self.docker.clone().with_timeout(needed)
        } else {
            self.docker.clone()
        }
    }
}

/// One runtime call on one object, for turning bollard's errors into [`RuntimeError`]s.
struct Call<'a> {
    operation: Operation,
    kind: ObjectKind,
    name: &'a str,
}

impl<'a> Call<'a> {
    fn new(operation: Operation, kind: ObjectKind, name: &'a str) -> Self {
        Self {
            operation,
            kind,
            name,
        }
    }

    fn error(&self, err: BollardError) -> RuntimeError {
        let name = self.name.to_owned();
        let kind = self.kind;
        match err {
            BollardError::DockerResponseServerError {
                status_code,
                message,
            } => match (self.operation, status_code) {
                (_, 404) => RuntimeError::NotFound { kind, name },
                (Operation::CreateContainer | Operation::EnsureNetwork, 409) => {
                    RuntimeError::Conflict { kind, name }
                }
                // Docker answers 403, podman 500 "network is being used".
                (Operation::RemoveNetwork, 403 | 409) => RuntimeError::InUse { kind, name },
                (Operation::RemoveNetwork, _) if Self::mentions(&message, &Self::IN_USE) => {
                    RuntimeError::InUse { kind, name }
                }
                (Operation::RemoveVolume | Operation::RemoveImage, 409) => {
                    RuntimeError::InUse { kind, name }
                }
                // Docker answers 409 "is not running", podman 500 "container state improper".
                (Operation::Exec, 409) => RuntimeError::NotRunning { name },
                (Operation::Exec, _) if Self::mentions(&message, &Self::NOT_RUNNING) => {
                    RuntimeError::NotRunning { name }
                }
                _ => RuntimeError::Api {
                    operation: self.operation,
                    message: format!("{kind} {name}: {message} (HTTP {status_code})"),
                },
            },
            other => RuntimeError::Api {
                operation: self.operation,
                message: format!("{kind} {name}: {other}"),
            },
        }
    }

    /// What a failed container create was missing, from podman's message: a missing image is
    /// a 404 "no such image: <ref>: image not known", but a missing network is a 500 "unable to
    /// find network with name or ID <name>: network not found" (podman 6.1). Only the phrases are
    /// matched, never a bare word, because image references may contain one ("…/networkd").
    fn missing_on_create(message: &str) -> Option<ObjectKind> {
        let message = message.to_ascii_lowercase();
        if message.contains("no such image") || message.contains("image not known") {
            Some(ObjectKind::Image)
        } else if message.contains("unable to find network")
            || message.contains("network not found")
        {
            Some(ObjectKind::Network)
        } else {
            None
        }
    }

    /// A failed create as a [`RuntimeError`], naming the missing image or network when that is
    /// what failed, and the container otherwise.
    fn create_error(&self, spec: &ContainerSpec, err: BollardError) -> RuntimeError {
        let missing = match &err {
            BollardError::DockerResponseServerError { message, .. } => {
                Self::missing_on_create(message)
            }
            _ => None,
        };
        match missing {
            Some(ObjectKind::Image) => RuntimeError::NotFound {
                kind: ObjectKind::Image,
                name: spec.image.to_string(),
            },
            Some(ObjectKind::Network) => RuntimeError::NotFound {
                kind: ObjectKind::Network,
                name: spec.network().to_owned(),
            },
            _ => self.error(err),
        }
    }

    /// How podman says a network is in use when it refuses to remove it.
    const IN_USE: [&str; 3] = ["being used", "in use", "active endpoints"];
    /// How podman says it refused an exec because the container is not running: "can only
    /// create exec sessions on running containers: container state improper" (podman 6.1).
    const NOT_RUNNING: [&str; 1] = ["container state improper"];

    /// Whether `message` contains one of `phrases`, ignoring case.
    fn mentions(message: &str, phrases: &[&str]) -> bool {
        let message = message.to_ascii_lowercase();
        phrases.iter().any(|phrase| message.contains(phrase))
    }
}

/// The options of every tmpfs mount (ADR-0059 §1, architecture §1.8): no `noexec`, and mode 1777
/// so an image's non-root user can write to a directory the image already has, which would
/// otherwise turn `root:root` under the mount.
const TMPFS_OPTIONS: &str = "rw,nosuid,nodev,mode=1777";

/// The create request, with every ADR-0019 setting pinned to its safe value: never privileged,
/// the stack network only (no host network), private PID/IPC namespaces, no devices, no added
/// capabilities, no binds. Ports go to 127.0.0.1 only (ADR-0007), podman never restarts a
/// container by itself (ADR-0028), and the log is a size-capped `k8s-file` (ADR-0044).
impl From<&ContainerSpec> for ContainerCreateBody {
    fn from(spec: &ContainerSpec) -> Self {
        let port_key = |port: u16| format!("{port}/tcp");
        let mut port_bindings: PortMap = PortMap::new();
        for binding in &spec.ports {
            port_bindings
                .entry(port_key(binding.container_port))
                .or_insert_with(|| Some(Vec::new()))
                .get_or_insert_with(Vec::new)
                .push(bollard::models::PortBinding {
                    host_ip: Some(PortBinding::HOST_IP.to_string()),
                    host_port: Some(binding.host_port.to_string()),
                });
        }
        let mut exposed: Vec<String> = spec
            .ports
            .iter()
            .map(|b| port_key(b.container_port))
            .collect();
        exposed.sort();
        exposed.dedup();
        let mounts = spec
            .volumes
            .iter()
            .map(|volume| Mount {
                typ: Some(MountType::VOLUME),
                source: Some(volume.volume().to_owned()),
                target: Some(volume.target().to_owned()),
                ..Mount::default()
            })
            .collect();
        let host_config = HostConfig {
            network_mode: Some(spec.network.clone()),
            port_bindings: Some(port_bindings),
            publish_all_ports: Some(false),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::NO),
                maximum_retry_count: None,
            }),
            mounts: Some(mounts),
            binds: None,
            volumes_from: None,
            privileged: Some(false),
            cap_add: None,
            devices: None,
            pid_mode: None,
            ipc_mode: None,
            uts_mode: None,
            userns_mode: None,
            readonly_rootfs: Some(spec.read_only),
            log_config: Some(ContainerLog::config()),
            // ADR-0059: each only when the service sets it, so podman keeps its defaults.
            shm_size: spec.shm_size.map(i64::from),
            tmpfs: (!spec.tmpfs.is_empty()).then(|| {
                spec.tmpfs
                    .iter()
                    .map(|(path, size)| {
                        let options = format!("{TMPFS_OPTIONS},size={}", u64::from(*size));
                        (path.to_string(), options)
                    })
                    .collect()
            }),
            init: spec.init.then_some(true),
            ..HostConfig::default()
        };
        // Podman would otherwise run a docker-format image's HEALTHCHECK itself, on systemd
        // timers; hoster runs every check (ADR-0038, ADR-0056).
        let healthcheck = HealthConfig {
            test: Some(vec!["NONE".to_owned()]),
            ..HealthConfig::default()
        };
        let endpoint = EndpointSettings {
            aliases: Some(vec![spec.network_alias.clone()]),
            ..EndpointSettings::default()
        };
        Self {
            image: Some(spec.image.to_string()),
            cmd: spec.command.clone(),
            entrypoint: spec.entrypoint.clone(),
            // Secrets last: the stack format already rejects a name set in both (ADR-0029).
            env: Some(
                spec.env
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .chain(
                        spec.secrets
                            .iter()
                            .map(|(k, v)| format!("{k}={}", v.expose())),
                    )
                    .collect(),
            ),
            labels: Some(spec.labels.clone().into_iter().collect()),
            user: spec.user.clone(),
            exposed_ports: Some(exposed),
            stop_timeout: Some(spec.stop_timeout.as_secs().try_into().unwrap_or(i64::MAX)),
            healthcheck: Some(healthcheck),
            host_config: Some(host_config),
            networking_config: Some(NetworkingConfig {
                endpoints_config: Some(HashMap::from([(spec.network.clone(), endpoint)])),
            }),
            ..ContainerCreateBody::default()
        }
    }
}

#[allow(
    dead_code,
    reason = "reached only through the runtime trait, not yet called"
)]
impl ContainerState {
    /// A state as podman reports it; an unknown one counts as dead (not running, needs
    /// attention).
    fn reported(status: &str, exit_code: Option<i64>) -> Self {
        match status.parse() {
            Ok(Self::Exited(None)) => Self::Exited(exit_code),
            Ok(state) => state,
            Err(_) => Self::Dead,
        }
    }
}

#[allow(
    dead_code,
    reason = "reached only through the runtime trait, not yet called"
)]
impl PublishedPort {
    /// A host IP as podman reports it; an empty one means all interfaces.
    fn reported_ip(ip: Option<&str>) -> IpAddr {
        ip.filter(|ip| !ip.is_empty())
            .and_then(|ip| ip.parse().ok())
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }
}

impl From<ContainerInspectResponse> for ContainerInfo {
    fn from(inspect: ContainerInspectResponse) -> Self {
        let state = inspect.state.unwrap_or_default();
        let status = state.status.map(|s| s.to_string()).unwrap_or_default();
        let ports = inspect
            .network_settings
            .and_then(|n| n.ports)
            .unwrap_or_default()
            .into_iter()
            .flat_map(|(key, bindings)| {
                let container_port = key.split('/').next().and_then(|p| p.parse().ok());
                bindings
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(move |binding| {
                        Some(PublishedPort {
                            container_port: container_port?,
                            host_ip: PublishedPort::reported_ip(binding.host_ip.as_deref()),
                            host_port: binding.host_port?.parse().ok()?,
                        })
                    })
            })
            .collect();
        Self::sorted(Self {
            id: inspect.id.unwrap_or_default().into(),
            name: inspect
                .name
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_owned(),
            labels: inspect
                .config
                .and_then(|c| c.labels)
                .unwrap_or_default()
                .into_iter()
                .collect(),
            state: ContainerState::reported(&status, state.exit_code),
            ports,
            started_at: state
                .started_at
                .as_deref()
                .and_then(ContainerInfo::reported_time),
            oom_killed: state.oom_killed.unwrap_or(false),
        })
    }
}

impl From<ContainerSummary> for ContainerInfo {
    fn from(summary: ContainerSummary) -> Self {
        let status = summary.state.map(|s| s.to_string()).unwrap_or_default();
        // The list endpoint has no exit code field; it is in the "Exited (137) 2s ago" text.
        let exit_code = summary.status.as_deref().and_then(|text| {
            let (_, rest) = text.split_once('(')?;
            rest.split_once(')')?.0.parse().ok()
        });
        let ports = summary
            .ports
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.typ.is_none_or(|t| t == PortSummaryTypeEnum::TCP))
            .filter_map(|p| {
                Some(PublishedPort {
                    container_port: p.private_port,
                    host_ip: PublishedPort::reported_ip(p.ip.as_deref()),
                    host_port: p.public_port?,
                })
            })
            .collect();
        Self::sorted(Self {
            id: summary.id.unwrap_or_default().into(),
            name: summary
                .names
                .unwrap_or_default()
                .into_iter()
                .next()
                .unwrap_or_default()
                .trim_start_matches('/')
                .to_owned(),
            labels: summary.labels.unwrap_or_default().into_iter().collect(),
            state: ContainerState::reported(&status, exit_code),
            ports,
            started_at: None,
            oom_killed: false,
        })
    }
}

#[allow(
    dead_code,
    reason = "reached only through the runtime trait, not yet called"
)]
impl ContainerInfo {
    /// A time podman reports (`State.StartedAt`, RFC 3339 with nanoseconds and an offset).
    /// Podman writes the zero time (`0001-01-01T00:00:00Z`) for a container that never
    /// started, which is no time at all.
    fn reported_time(stamp: &str) -> Option<SystemTime> {
        let time = SystemTime::from(OffsetDateTime::parse(stamp, &Rfc3339).ok()?);
        (time > UNIX_EPOCH).then_some(time)
    }

    fn sorted(mut self) -> Self {
        self.ports.sort();
        self.ports.dedup();
        self
    }
}

/// The file system of one created container, as podman stats it.
struct ContainerPaths<'a> {
    runtime: &'a PodmanRuntime,
    id: &'a ContainerId,
}

impl StatPath for ContainerPaths<'_> {
    async fn stat(&self, path: &str) -> Result<PathStat, RuntimeError> {
        self.runtime.stat(self.id, path).await
    }
}

/// Writing config files into a created container (ADR-0057 §3, architecture §8).
impl PodmanRuntime {
    /// How long one stat of a container path may take.
    const STAT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Uploads `spec.files` into the created, unstarted container `id`: resolves each path
    /// through the image's symbolic links, checks the resolved paths, then sends the
    /// directories and the files with numeric owners, and the container user's files with
    /// `copyUIDGID`. Neither upload replaces a directory with a file or the other way round.
    async fn upload_files(
        &self,
        id: &ContainerId,
        spec: &ContainerSpec,
    ) -> Result<(), RuntimeError> {
        if spec.files.is_empty() {
            return Ok(());
        }
        let mut volumes: Vec<String> = spec.volumes.iter().map(|v| v.target().to_owned()).collect();
        volumes.extend(self.image_volumes(&spec.image).await?);
        let paths = ContainerPaths { runtime: self, id };
        let layout = Layout::resolve(&spec.files, &volumes, &paths).await?;
        let mtime = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let archives = layout.archives(mtime).map_err(|err| RuntimeError::Api {
            operation: Operation::UploadFiles,
            message: format!("cannot pack the files of {}: {err}", spec.name),
        })?;
        for (archive, copy_uidgid) in [(archives.ids, false), (archives.container_user, true)] {
            let Some(archive) = archive else {
                continue;
            };
            let mut options = UploadToContainerOptionsBuilder::new()
                .path("/")
                .no_overwrite_dir_non_dir("true");
            if copy_uidgid {
                options = options.copy_uidgid("true");
            }
            self.docker
                .upload_to_container(
                    id.as_ref(),
                    Some(options.build()),
                    body_full(archive.into()),
                )
                .await
                .map_err(|err| match err {
                    BollardError::DockerResponseServerError { message, .. } => {
                        UploadFailure(&message).into()
                    }
                    err => Call::new(Operation::UploadFiles, ObjectKind::Container, &spec.name)
                        .error(err),
                })?;
        }
        tracing::debug!(container = %spec.name, files = spec.files.len(), "config files written");
        Ok(())
    }

    /// `HEAD /containers/{id}/archive?path=`, a raw HTTP call over the podman socket next to
    /// bollard: its own call fails on a 404, which is how podman answers for a missing path
    /// and, with the stat header, for a dangling link.
    async fn stat(&self, id: &ContainerId, path: &str) -> Result<PathStat, RuntimeError> {
        let failed = |message: String| RuntimeError::Api {
            operation: Operation::UploadFiles,
            message,
        };
        let query: String = form_urlencoded::byte_serialize(path.as_bytes()).collect();
        let request = format!(
            "HEAD /containers/{}/archive?path={query} HTTP/1.1\r\nHost: d\r\n\
             Connection: close\r\n\r\n",
            id.as_ref()
        );
        let exchange = async {
            let mut stream = tokio::net::UnixStream::connect(&self.socket).await?;
            stream.write_all(request.as_bytes()).await?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await?;
            std::io::Result::Ok(response)
        };
        let response = match tokio::time::timeout(Self::STAT_TIMEOUT, exchange).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => return Err(failed(format!("cannot stat `{path}`: {err}"))),
            Err(_) => return Err(failed(format!("podman did not answer a stat of `{path}`"))),
        };
        let response = String::from_utf8_lossy(&response);
        let mut lines = response.lines();
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .ok_or_else(|| failed(format!("podman's answer to a stat of `{path}` is not HTTP")))?;
        let header = lines
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case(PathStat::HEADER))
            .map(|(_, value)| value.trim());
        PathStat::from_response(path, status, header).map_err(failed)
    }
}

impl ContainerRuntime for PodmanRuntime {
    /// Creates `hoster-<stack>` unless it exists. A same-named network hoster did not create is
    /// a [`RuntimeError::Conflict`], never adopted.
    async fn ensure_network(&self, network: &NetworkSpec) -> Result<(), RuntimeError> {
        let call = Call::new(Operation::EnsureNetwork, ObjectKind::Network, &network.name);
        match self.docker.inspect_network(&network.name, None).await {
            Ok(existing) => {
                let managed = existing
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get(label::MANAGED))
                    .is_some_and(|v| v == "true");
                return if managed {
                    Ok(())
                } else {
                    Err(RuntimeError::Conflict {
                        kind: ObjectKind::Network,
                        name: network.name.clone(),
                    })
                };
            }
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(err) => return Err(call.error(err)),
        }
        let request = NetworkCreateRequest {
            name: network.name.clone(),
            driver: Some("bridge".to_owned()),
            labels: Some(network.labels.clone().into_iter().collect()),
            ..NetworkCreateRequest::default()
        };
        match self.docker.create_network(request).await {
            Ok(_) => Ok(()),
            // Created concurrently since the inspect: that is the state we wanted.
            Err(BollardError::DockerResponseServerError {
                status_code: 409, ..
            }) => Ok(()),
            Err(err) => Err(call.error(err)),
        }
    }

    async fn ensure_volume(&self, volume: &VolumeSpec) -> Result<(), RuntimeError> {
        let call = Call::new(Operation::EnsureVolume, ObjectKind::Volume, &volume.name);
        match self.docker.inspect_volume(&volume.name).await {
            Ok(existing) => return volume.check_existing(&existing.labels),
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(err) => return Err(call.error(err)),
        }
        let request = VolumeCreateRequest {
            name: Some(volume.name.clone()),
            labels: Some(volume.labels.clone().into_iter().collect()),
            ..VolumeCreateRequest::default()
        };
        match self.docker.create_volume(request).await {
            Ok(_) => Ok(()),
            // Created concurrently since the inspect: that is the state we wanted.
            Err(BollardError::DockerResponseServerError {
                status_code: 409, ..
            }) => Ok(()),
            Err(err) => Err(call.error(err)),
        }
    }

    async fn remove_network(&self, name: &str) -> Result<(), RuntimeError> {
        let call = Call::new(Operation::RemoveNetwork, ObjectKind::Network, name);
        self.docker
            .remove_network(name)
            .await
            .map_err(|err| call.error(err))
    }

    async fn list_managed_networks(&self) -> Result<Vec<NetworkSpec>, RuntimeError> {
        let call = Call::new(Operation::ListNetworks, ObjectKind::Network, "list");
        let filters = HashMap::from([("label", vec![format!("{}=true", label::MANAGED)])]);
        let options = ListNetworksOptionsBuilder::new().filters(&filters).build();
        let mut networks: Vec<NetworkSpec> = self
            .docker
            .list_networks(Some(options))
            .await
            .map_err(|err| call.error(err))?
            .into_iter()
            .filter_map(|network| {
                Some(NetworkSpec {
                    name: network.name?,
                    labels: network.labels.unwrap_or_default().into_iter().collect(),
                })
            })
            // The label filter is podman's; check it again rather than trust it.
            .filter(NetworkSpec::is_managed)
            .collect();
        networks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(networks)
    }

    async fn pull_image(&self, image: &PinnedImage) -> Result<(), RuntimeError> {
        // Always `registry/repository@digest`: a digest names exactly one image, so presence
        // is final and the pull never resolves a tag.
        let reference = image.to_string();
        if self.has_image(&reference).await? {
            tracing::debug!(image = %reference, "image already present, not pulling");
            return Ok(());
        }
        let credential = self.credentials.lookup(image.registry()).await?;
        tracing::info!(
            image = %reference,
            authenticated = credential.is_some(),
            "pulling image"
        );
        let auth = credential
            .as_ref()
            .map(|c| PullAuth(image.registry(), c).into());
        let options = CreateImageOptionsBuilder::new()
            .from_image(&reference)
            .build();
        let mut progress = self.docker.create_image(Some(options), None, auth);
        while let Some(event) = progress.next().await {
            match event {
                Ok(info) => tracing::debug!(
                    image = %reference,
                    layer = info.id.as_deref().unwrap_or(""),
                    status = info.status.as_deref().unwrap_or(""),
                    current = info.progress_detail.as_ref().and_then(|p| p.current),
                    total = info.progress_detail.as_ref().and_then(|p| p.total),
                    "pull progress"
                ),
                Err(err) => {
                    let failure = PullFailure::from(err).redact(credential.as_ref());
                    return Err(failure.into_error(image));
                }
            }
        }
        tracing::info!(image = %reference, "image pulled");
        Ok(())
    }

    async fn remove_image(&self, image: &PinnedImage) -> Result<(), RuntimeError> {
        let reference = image.to_string();
        let call = Call::new(Operation::RemoveImage, ObjectKind::Image, &reference);
        // Not forced: an image a container still uses is reported as in use (409) and kept.
        // An image other references name too (a tag, another digest) only loses this name.
        let options = RemoveImageOptionsBuilder::new().force(false).build();
        let removed = self
            .docker
            .remove_image(&reference, Some(options), None)
            .await
            .map_err(|err| call.error(err))?;
        if removed.iter().any(|item| item.deleted.is_some()) {
            tracing::info!(image = %reference, "image removed");
        } else {
            tracing::info!(image = %reference, "image untagged; other names keep it");
        }
        Ok(())
    }

    async fn create_container(&self, spec: &ContainerSpec) -> Result<ContainerId, RuntimeError> {
        let call = Call::new(
            Operation::CreateContainer,
            ObjectKind::Container,
            &spec.name,
        );
        let options = CreateContainerOptionsBuilder::new()
            .name(&spec.name)
            .build();
        let created = self
            .docker
            .create_container(Some(options), spec.into())
            .await
            .map_err(|err| call.create_error(spec, err))?;
        for warning in &created.warnings {
            tracing::warn!(container = %spec.name, %warning, "podman warned on create");
        }
        let id = ContainerId::from(created.id);
        if let Err(err) = self.upload_files(&id, spec).await {
            // A container with half its files must never start: nothing else knows it exists
            // yet, so it is removed here.
            if let Err(removal) = self.remove(&id).await {
                tracing::warn!(container = %spec.name, error = %removal, "cannot remove a container whose config files could not be written");
            }
            return Err(err);
        }
        Ok(id)
    }

    async fn image_volumes(&self, image: &PinnedImage) -> Result<Vec<String>, RuntimeError> {
        let reference = image.to_string();
        let call = Call::new(Operation::ImageVolumes, ObjectKind::Image, &reference);
        let inspected = self
            .docker
            .inspect_image(&reference)
            .await
            .map_err(|err| call.error(err))?;
        let mut volumes = inspected
            .config
            .and_then(|config| config.volumes)
            .unwrap_or_default();
        volumes.sort();
        Ok(volumes)
    }

    async fn start(&self, id: &ContainerId) -> Result<(), RuntimeError> {
        let call = Call::new(Operation::Start, ObjectKind::Container, id.as_ref());
        // Starting a running container is a 304, which bollard treats as success.
        self.docker
            .start_container(id.as_ref(), None)
            .await
            .map_err(|err| call.error(err))
    }

    async fn stop(&self, id: &ContainerId, timeout: Duration) -> Result<(), RuntimeError> {
        let call = Call::new(Operation::Stop, ObjectKind::Container, id.as_ref());
        let seconds = i32::try_from(timeout.as_secs()).unwrap_or(i32::MAX);
        let options = StopContainerOptionsBuilder::new().t(seconds).build();
        // Stopping a stopped container is a 304, which bollard treats as success.
        self.client_for_stop(timeout)
            .stop_container(id.as_ref(), Some(options))
            .await
            .map_err(|err| call.error(err))
    }

    async fn remove(&self, id: &ContainerId) -> Result<(), RuntimeError> {
        let call = Call::new(Operation::Remove, ObjectKind::Container, id.as_ref());
        // Named volumes are kept (ADR-0030): `v` only concerns anonymous ones, and hoster never
        // creates those, but leave them alone too.
        let options = RemoveContainerOptionsBuilder::new()
            .force(true)
            .v(false)
            .build();
        self.docker
            .remove_container(id.as_ref(), Some(options))
            .await
            .map_err(|err| call.error(err))
    }

    async fn inspect(&self, id: &ContainerId) -> Result<ContainerInfo, RuntimeError> {
        let call = Call::new(Operation::Inspect, ObjectKind::Container, id.as_ref());
        self.docker
            .inspect_container(id.as_ref(), None)
            .await
            .map(ContainerInfo::from)
            .map_err(|err| call.error(err))
    }

    async fn remove_volume(&self, name: &str) -> Result<(), RuntimeError> {
        let call = Call::new(Operation::RemoveVolume, ObjectKind::Volume, name);
        // Not forced: a volume a container still mounts is reported as in use.
        self.docker
            .remove_volume(name, None::<RemoveVolumeOptions>)
            .await
            .map_err(|err| call.error(err))
    }

    async fn list_managed(&self) -> Result<Vec<ContainerInfo>, RuntimeError> {
        let call = Call::new(Operation::ListManaged, ObjectKind::Container, "list");
        let filters = HashMap::from([("label", vec![format!("{}=true", label::MANAGED)])]);
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let mut containers: Vec<ContainerInfo> = self
            .docker
            .list_containers(Some(options))
            .await
            .map_err(|err| call.error(err))?
            .into_iter()
            .map(ContainerInfo::from)
            // The label filter is podman's; check it again rather than trust it.
            .filter(ContainerInfo::is_managed)
            .collect();
        containers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(containers)
    }

    fn version(&self) -> String {
        self.version.clone()
    }

    /// podman's compat `/info`: its version, whether it runs rootless (a `name=rootless`
    /// security option) and the cgroups version and manager.
    async fn engine(&self) -> Result<EngineInfo, RuntimeError> {
        let info = self
            .docker
            .info()
            .await
            .map_err(|err| RuntimeError::Unreachable {
                socket: self.socket.display().to_string(),
                message: err.to_string(),
            })?;
        let named = |value: String| Some(value).filter(|v| !v.is_empty());
        Ok(EngineInfo {
            version: info.server_version.and_then(named),
            rootless: info
                .security_options
                .map(|options| options.iter().any(|o| o == "name=rootless")),
            cgroup_version: info.cgroup_version.map(|v| v.to_string()).and_then(named),
            cgroup_driver: info.cgroup_driver.map(|d| d.to_string()).and_then(named),
        })
    }

    /// Creates an exec session through the compat API, starts it attached and collects its
    /// output until the output closes and exec inspect reports an exit code, or until `timeout`
    /// passes (the whole call, create included). The session is removed in the background; a
    /// non-zero exit from a container that is no longer running is [`RuntimeError::NotRunning`].
    /// An exec still running at the timeout, or still being started then, is handed to a
    /// background task that lets the start finish, tries to end it ([`ExecProcess`]), waits
    /// until it has ended and then removes its session.
    async fn exec(
        &self,
        id: &ContainerId,
        argv: &[String],
        timeout: Duration,
    ) -> Result<ExecOutcome, RuntimeError> {
        let argv = Argv::try_from(argv)?;
        let deadline = Instant::now() + timeout;
        let call = Call::new(Operation::Exec, ObjectKind::Container, id.as_ref());
        // Neither request is ever cut off: podman runs the command even when the start's
        // connection drops, and then reports the session as exited 125 without a PID (§1.6).
        // One still in flight at the deadline finishes in the background.
        let mut create = self.spawn_create(id, argv);
        let Ok(created) = tokio::time::timeout_at(deadline, &mut create).await else {
            // Nothing has started; a session created late is removed unstarted.
            let runtime = self.clone();
            tokio::spawn(async move {
                if let Ok(session) = joined(create.await) {
                    runtime.sessions.remove(&runtime.socket, session).await;
                }
            });
            let (exec, _ended) = RunningExec::new();
            return Ok(ExecOutcome::TimedOut {
                output: ExecOutput::default(),
                exec,
            });
        };
        let session = joined(created)?;
        let mut start = self.spawn_start(id, &session);
        let Ok(started) = tokio::time::timeout_at(deadline, &mut start).await else {
            tracing::debug!(container = %id, %session, "exec start timed out");
            return Ok(self.timed_out(session, ExecStart::Pending(start), ExecOutput::default()));
        };
        let (mut reader, tail) = match joined(started) {
            Ok(attached) => attached,
            Err(err) => {
                self.remove_later(session);
                return Err(err);
            }
        };
        let code = match tokio::time::timeout_at(deadline, &mut reader).await {
            Ok(_) => self.exit_code(&call, &session, deadline).await,
            Err(_) => Ok(None),
        };
        let output = tail.lock().expect("never poisoned").snapshot();
        let code = match code {
            Ok(Some(code)) => code,
            Ok(None) => {
                tracing::debug!(container = %id, %session, "exec timed out");
                return Ok(self.timed_out(session, ExecStart::Started(reader), output));
            }
            Err(err) => {
                self.remove_later(session);
                return Err(err);
            }
        };
        self.remove_later(session);
        if code != 0 {
            // The deadline bounds this too; without an answer the exit stands as it is.
            if let Ok(info) = tokio::time::timeout_at(deadline, self.inspect(id)).await
                && !info?.state.is_running()
            {
                return Err(RuntimeError::NotRunning {
                    name: id.to_string(),
                });
            }
        }
        Ok(ExecOutcome::finished(code, output))
    }

    /// Reads the log through the compat `logs` endpoint, with timestamps, and joins podman's
    /// frames into lines (see [`ContainerLog`]). Dropping the stream closes the request, so a
    /// followed log stops being read as soon as its reader goes away.
    fn logs(
        &self,
        id: &ContainerId,
        options: LogOptions,
    ) -> BoxStream<'static, Result<LogLine, RuntimeError>> {
        let name = id.to_string();
        let frames = self
            .docker
            .logs(id.as_ref(), Some(ContainerLog::query(options)));
        let mut log = ContainerLog::default();
        frames
            .map(Some)
            // A sentinel after the last frame, to flush a final line without a newline.
            .chain(stream::once(async { None }))
            .flat_map(move |frame| {
                let lines: Vec<_> = match frame {
                    Some(Ok(frame)) => log.push(frame).into_iter().map(Ok).collect(),
                    Some(Err(err)) => {
                        let call = Call::new(Operation::Logs, ObjectKind::Container, &name);
                        vec![Err(call.error(err))]
                    }
                    None => log.flush().into_iter().map(Ok).collect(),
                };
                stream::iter(lines)
            })
            .boxed()
    }
}

/// The task reading an attached exec's output into the tail; it finishes when the output
/// closes, whether because the exec ended or because the stream failed.
type ExecReader = JoinHandle<()>;

/// An attached exec: its reader and the tail it reads into.
type Attached = (ExecReader, Arc<Mutex<OutputTail>>);

/// How far a timed-out exec got.
enum ExecStart {
    /// The start request was still in flight at the timeout.
    Pending(JoinHandle<Result<Attached, RuntimeError>>),
    Started(ExecReader),
}

/// The result of a task this module spawned; a panic in it carries on in the caller.
fn joined<T>(result: Result<T, tokio::task::JoinError>) -> T {
    result.unwrap_or_else(|err| std::panic::resume_unwind(err.into_panic()))
}

impl PodmanRuntime {
    /// How often a timed-out exec is inspected while it still runs.
    const EXEC_ENDED_POLL: Duration = Duration::from_secs(1);

    /// [`Self::create_exec`] as a task of its own, so the deadline never cuts it off.
    fn spawn_create(
        &self,
        id: &ContainerId,
        argv: Argv<'_>,
    ) -> JoinHandle<Result<String, RuntimeError>> {
        let runtime = self.clone();
        let container = id.to_string();
        let argv = argv.as_ref().to_vec();
        tokio::spawn(async move {
            let call = Call::new(Operation::Exec, ObjectKind::Container, &container);
            runtime.create_exec(&call, Argv::try_from(&argv[..])?).await
        })
    }

    /// [`Self::start_exec`] as a task of its own, so the deadline never cuts it off.
    fn spawn_start(
        &self,
        id: &ContainerId,
        exec: &str,
    ) -> JoinHandle<Result<Attached, RuntimeError>> {
        let runtime = self.clone();
        let container = id.to_string();
        let exec = exec.to_owned();
        tokio::spawn(async move {
            let call = Call::new(Operation::Exec, ObjectKind::Container, &container);
            runtime.start_exec(&call, &exec).await
        })
    }

    /// Creates an exec session running `argv` and returns its id.
    async fn create_exec(&self, call: &Call<'_>, argv: Argv<'_>) -> Result<String, RuntimeError> {
        let created = self
            .docker
            .create_exec(call.name, ExecRequest(argv))
            .await
            .map_err(|err| call.error(err))?;
        Ok(created.id)
    }

    /// Starts the exec session `exec` attached, and the task that reads its output into the
    /// returned tail.
    async fn start_exec(&self, call: &Call<'_>, exec: &str) -> Result<Attached, RuntimeError> {
        let options = StartExecOptions {
            detach: false,
            tty: false,
            output_capacity: None,
        };
        let started =
            self.docker
                .start_exec(exec, Some(options))
                .await
                .map_err(|err| match err {
                    // The session went between create and start, with its container.
                    BollardError::DockerResponseServerError {
                        status_code: 404, ..
                    } => RuntimeError::NotRunning {
                        name: call.name.to_owned(),
                    },
                    other => call.error(other),
                })?;
        let StartExecResults::Attached { mut output, input } = started else {
            return Err(RuntimeError::Api {
                operation: Operation::Exec,
                message: format!("container {}: podman started the exec detached", call.name),
            });
        };
        // No stdin: closing our end tells nobody anything, but holding it serves no one.
        drop(input);
        let tail = Arc::new(Mutex::new(OutputTail::default()));
        let reader = tokio::spawn({
            let tail = Arc::clone(&tail);
            let exec = exec.to_owned();
            async move {
                while let Some(frame) = output.next().await {
                    match frame {
                        Ok(
                            LogOutput::StdOut { message }
                            | LogOutput::StdErr { message }
                            | LogOutput::Console { message },
                        ) => tail.lock().expect("never poisoned").push(&message),
                        Ok(LogOutput::StdIn { .. }) => {}
                        Err(err) => {
                            // The exec may still run; exec inspect tells.
                            tracing::debug!(%exec, error = %err, "exec output ended with an error");
                            break;
                        }
                    }
                }
            }
        });
        Ok((reader, tail))
    }

    /// The exit code of an exec whose output has closed, read from exec inspect until `until`
    /// (see [`ExitPoll`]); `None` when it still runs then.
    async fn exit_code(
        &self,
        call: &Call<'_>,
        exec: &str,
        until: Instant,
    ) -> Result<Option<i64>, RuntimeError> {
        let mut poll = ExitPoll::new(call, exec, until);
        loop {
            let Ok(inspect) = tokio::time::timeout_at(until, self.docker.inspect_exec(exec)).await
            else {
                return Ok(None);
            };
            match poll.step(inspect, Instant::now()) {
                ControlFlow::Break(code) => return code,
                ControlFlow::Continue(wait) => tokio::time::sleep(wait).await,
            }
        }
    }

    /// The outcome of an exec that is, or may be, still running at its timeout: a background
    /// task ([`Self::end_timed_out`]) holds the [`RunningExec`]'s guard until it has ended.
    fn timed_out(&self, exec: String, start: ExecStart, output: ExecOutput) -> ExecOutcome {
        let (running, guard) = RunningExec::new();
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime.end_timed_out(exec, start, guard).await;
        });
        ExecOutcome::TimedOut {
            output,
            exec: running,
        }
    }

    /// Waits for a pending start, ends a timed-out exec if it can, waits until it has ended
    /// (killed, finished, or gone with its container), removes the session and then drops
    /// `guard`, which tells the [`RunningExec`] watchers.
    async fn end_timed_out(self, exec: String, start: ExecStart, guard: ExecGuard) {
        let reader = match start {
            ExecStart::Started(reader) => reader,
            ExecStart::Pending(start) => match joined(start.await) {
                Ok((reader, _)) => reader,
                Err(err) => {
                    tracing::debug!(%exec, error = %err, "a timed-out exec never started");
                    self.sessions.remove(&self.socket, exec).await;
                    return;
                }
            },
        };
        if self.end_exec(&exec).await {
            tracing::debug!(%exec, "ended a timed-out exec");
        }
        // A panicked reader has no output left to read either.
        let _ = reader.await;
        // The output also closes when its stream fails, while the exec may run on.
        while self.exec_runs(&exec).await {
            tokio::time::sleep(Self::EXEC_ENDED_POLL).await;
        }
        self.sessions.remove(&self.socket, exec).await;
        drop(guard);
    }

    /// Tries to end the exec ([`Self::kill_exec`]); whether it signalled it.
    async fn end_exec(&self, exec: &str) -> bool {
        self.kill_exec(exec).await.unwrap_or_else(|err| {
            tracing::info!(
                %exec,
                error = %err,
                "cannot end a timed-out exec; it runs until it ends or its container stops"
            );
            false
        })
    }

    /// Whether exec inspect says the session still runs. A session that is gone does not; any
    /// other failure counts as running, so an exec is never taken for ended without podman
    /// saying so.
    async fn exec_runs(&self, exec: &str) -> bool {
        match self.docker.inspect_exec(exec).await {
            Ok(inspect) => inspect.running == Some(true),
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => false,
            Err(err) => {
                tracing::debug!(%exec, error = %err, "cannot inspect a timed-out exec");
                true
            }
        }
    }

    /// Signals a running exec's process group (architecture §5.5): the PID is read while the
    /// session runs, pinned with a pidfd, and checked again against the session before
    /// anything is signalled, so a PID reused after the exec ended is never touched. `false`
    /// when the session was not running.
    async fn kill_exec(&self, exec: &str) -> io::Result<bool> {
        let running = |inspect: &ExecInspectResponse| {
            (inspect.running == Some(true))
                .then_some(inspect.pid)
                .flatten()
        };
        let inspect = self
            .docker
            .inspect_exec(exec)
            .await
            .map_err(io::Error::other)?;
        let Some(pid) = running(&inspect) else {
            return Ok(false);
        };
        let context = |what: &'static str| {
            move |err: io::Error| io::Error::new(err.kind(), format!("{what} PID {pid}: {err}"))
        };
        let process = ExecProcess::open(pid).map_err(context("cannot open its"))?;
        let again = self
            .docker
            .inspect_exec(exec)
            .await
            .map_err(io::Error::other)?;
        if running(&again) != Some(process.pid()) {
            return Ok(false);
        }
        process
            .terminate()
            .await
            .map_err(context("cannot signal"))?;
        Ok(true)
    }

    /// Removes the finished session `exec` in the background, so a probe never waits on it.
    fn remove_later(&self, exec: String) {
        let sessions = Arc::clone(&self.sessions);
        let socket = self.socket.clone();
        tokio::spawn(async move { sessions.remove(&socket, exec).await });
    }
}

/// Reading a finished exec's exit code from successive exec inspects, until a deadline. Podman
/// may report an exec `Running` for a moment after its output closed, and for as long as it
/// really runs when the output stream failed instead; a session that is gone went with its
/// container (a stop, restart or removal).
struct ExitPoll<'a> {
    call: &'a Call<'a>,
    exec: &'a str,
    until: Instant,
    wait: Duration,
}

impl<'a> ExitPoll<'a> {
    /// The first wait between inspects, doubled after each up to [`Self::MAX_WAIT`].
    const FIRST_WAIT: Duration = Duration::from_millis(20);
    const MAX_WAIT: Duration = Duration::from_millis(250);

    fn new(call: &'a Call<'a>, exec: &'a str, until: Instant) -> Self {
        Self {
            call,
            exec,
            until,
            wait: Self::FIRST_WAIT,
        }
    }

    /// What one inspect, answered at `now`, decides: the exit code, `None` when the exec still
    /// runs at the deadline, an error, or how long to wait before inspecting again.
    fn step(
        &mut self,
        inspect: Result<ExecInspectResponse, BollardError>,
        now: Instant,
    ) -> ControlFlow<Result<Option<i64>, RuntimeError>, Duration> {
        let (call, exec) = (self.call, self.exec);
        match inspect {
            Ok(inspect) if inspect.running != Some(true) => ControlFlow::Break(
                inspect
                    .exit_code
                    .map(Some)
                    .ok_or_else(|| RuntimeError::Api {
                        operation: Operation::Exec,
                        message: format!("container {}: exec {exec} has no exit code", call.name),
                    }),
            ),
            Ok(_) if now >= self.until => ControlFlow::Break(Ok(None)),
            Ok(_) => {
                let wait = self.wait.min(self.until - now);
                self.wait = (self.wait * 2).min(Self::MAX_WAIT);
                ControlFlow::Continue(wait)
            }
            Err(BollardError::DockerResponseServerError {
                status_code: 404, ..
            }) => ControlFlow::Break(Err(RuntimeError::NotRunning {
                name: call.name.to_owned(),
            })),
            Err(err) => ControlFlow::Break(Err(call.error(err))),
        }
    }
}

/// The compat exec create body: `argv` without a shell, with stdout and stderr attached and no
/// stdin or TTY. `Env`, `User`, `WorkingDir` and `Privileged` are left out, so the command runs
/// as the container's user, with its environment, in its working directory.
struct ExecRequest<'a>(Argv<'a>);

impl From<ExecRequest<'_>> for ExecConfig {
    fn from(ExecRequest(argv): ExecRequest<'_>) -> Self {
        Self {
            attach_stdin: Some(false),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            tty: Some(false),
            cmd: Some(argv.as_ref().to_vec()),
            ..Self::default()
        }
    }
}

/// Removing finished exec sessions through libpod's `POST /libpod/exec/{id}/remove`, a raw HTTP
/// call over the podman socket next to bollard (architecture §5.5). Best effort: podman removes
/// a finished compat session itself after `exit_command_delay` (300 s by default), which bounds
/// them anyway, so a podman without the route (before 4.9) is no error.
#[derive(Debug, Default)]
struct ExecSessions {
    /// Sessions whose removal failed, retried with the next removal.
    pending: Mutex<Vec<String>>,
    /// Set once podman has answered that it has no such route.
    unsupported: AtomicBool,
}

impl ExecSessions {
    /// The libpod API version in the path. Podman serves libpod routes only under a version
    /// prefix (an unversioned path is a plain-text 404, like a missing route) and accepts any
    /// version it supports; 4.0.0 is the oldest libpod API with the v4 layout.
    const LIBPOD_API: &str = "v4.0.0";
    /// How long one removal may take.
    const TIMEOUT: Duration = Duration::from_secs(10);
    /// The most failed removals kept for a retry; older ones are left to podman.
    const MAX_PENDING: usize = 64;

    /// Removes `exec`, and any session whose removal failed before. Runs in the background
    /// ([`PodmanRuntime::remove_later`]): a slow podman delays only this.
    async fn remove(&self, socket: &Path, exec: String) {
        if self.unsupported.load(Ordering::Relaxed) {
            return;
        }
        let mut sessions = std::mem::take(&mut *self.pending.lock().expect("never poisoned"));
        sessions.push(exec);
        let mut failed = Vec::new();
        for session in sessions {
            match Self::request(socket, &session).await {
                SessionRemoval::Removed | SessionRemoval::Gone => {}
                SessionRemoval::Unsupported => {
                    tracing::info!(
                        "podman cannot remove exec sessions (before 4.9); it removes finished \
                         ones itself after exit_command_delay"
                    );
                    self.unsupported.store(true, Ordering::Relaxed);
                    return;
                }
                SessionRemoval::Failed(reason) => {
                    tracing::debug!(exec = %session, %reason, "cannot remove an exec session; retrying later");
                    failed.push(session);
                }
            }
        }
        let mut pending = self.pending.lock().expect("never poisoned");
        pending.extend(failed);
        let excess = pending.len().saturating_sub(Self::MAX_PENDING);
        pending.drain(..excess);
    }

    /// One `POST /v4.0.0/libpod/exec/{id}/remove`, as HTTP/1.1 with `Connection: close`.
    async fn request(socket: &Path, exec: &str) -> SessionRemoval {
        let request = format!(
            "POST /{}/libpod/exec/{exec}/remove HTTP/1.1\r\nHost: d\r\n\
             Content-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}",
            Self::LIBPOD_API
        );
        let exchange = async {
            let mut stream = tokio::net::UnixStream::connect(socket).await?;
            stream.write_all(request.as_bytes()).await?;
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await?;
            std::io::Result::Ok(response)
        };
        match tokio::time::timeout(Self::TIMEOUT, exchange).await {
            Ok(Ok(response)) => SessionRemoval::from(String::from_utf8_lossy(&response).as_ref()),
            Ok(Err(err)) => SessionRemoval::Failed(err.to_string()),
            Err(_) => SessionRemoval::Failed("podman did not answer in time".to_owned()),
        }
    }
}

/// What podman answered to an exec session removal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionRemoval {
    Removed,
    /// A JSON 404 with cause `no such exec session`: podman's own cleanup, or a container
    /// restart or removal, got there first.
    Gone,
    /// A plain-text 404 `Not Found`: podman has no such route (before 4.9).
    Unsupported,
    Failed(String),
}

/// Reads a raw HTTP/1.1 response: its status line and, for a 404, whether the body is libpod's
/// JSON error or the router's plain text.
impl From<&str> for SessionRemoval {
    fn from(response: &str) -> Self {
        let status = response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok());
        let body = response.split_once("\r\n\r\n").map_or("", |(_, body)| body);
        match status {
            Some(200..=299) => Self::Removed,
            Some(404) if body.contains("no such exec session") => Self::Gone,
            Some(404) => Self::Unsupported,
            Some(status) => Self::Failed(format!("HTTP {status}: {}", body.trim())),
            None => Self::Failed("not an HTTP response".to_owned()),
        }
    }
}

/// A container's log as podman keeps and serves it (ADR-0044): written by the `k8s-file` driver
/// with a size cap set at create, and read back through the compat `logs` endpoint.
///
/// Read with timestamps, podman sends one frame per log entry, prefixed with the entry's RFC 3339
/// time (in the host's offset, not necessarily UTC). Output without a trailing newline, or a line
/// longer than conmon's buffer, arrives as several partial frames; this joins them per stream
/// until the newline, so a [`LogLine`] is always one whole line.
#[derive(Debug, Default)]
struct ContainerLog {
    stdout: Option<PendingLine>,
    stderr: Option<PendingLine>,
}

/// The start of a line whose newline has not arrived yet.
#[derive(Debug)]
struct PendingLine {
    time: Option<SystemTime>,
    text: String,
}

impl PendingLine {
    fn new(time: Option<SystemTime>) -> Self {
        Self {
            time,
            text: String::new(),
        }
    }

    /// The finished line, with `rest` appended and a trailing `\r` dropped.
    fn finish(mut self, stream: LogStream, rest: &str) -> LogLine {
        self.text.push_str(rest);
        if self.text.ends_with('\r') {
            self.text.pop();
        }
        LogLine {
            stream,
            time: self.time,
            text: self.text,
        }
    }
}

impl ContainerLog {
    /// The log driver every container is created with.
    const DRIVER: &str = "k8s-file";
    /// The most log podman keeps per container. Beyond it, podman truncates the file and starts
    /// over, so a chatty service cannot fill the disk.
    const MAX_SIZE: &str = "10m";

    /// The `LogConfig` of a container create request.
    fn config() -> HostConfigLogConfig {
        HostConfigLogConfig {
            typ: Some(Self::DRIVER.to_owned()),
            config: Some(HashMap::from([(
                "max-size".to_owned(),
                Self::MAX_SIZE.to_owned(),
            )])),
        }
    }

    /// The query of a `logs` request: both streams, with timestamps, and `since` rounded down to
    /// the whole second the API takes.
    fn query(options: LogOptions) -> LogsOptions {
        let mut query = LogsOptionsBuilder::new()
            .follow(options.follow)
            .stdout(true)
            .stderr(true)
            .timestamps(true);
        if let Some(tail) = options.tail {
            query = query.tail(&tail.to_string());
        }
        if let Some(since) = options.since {
            let seconds = since.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
            // 0 means "no limit" to the API; the epoch itself is as good as no limit.
            query = query.since(i32::try_from(seconds).unwrap_or(i32::MAX));
        }
        query.build()
    }

    /// The lines that `frame` completes.
    fn push(&mut self, frame: LogOutput) -> Vec<LogLine> {
        let (stream, bytes) = match frame {
            // A TTY has no separate stderr; hoster does not allocate one, but be lenient.
            LogOutput::StdOut { message } | LogOutput::Console { message } => {
                (LogStream::Stdout, message)
            }
            LogOutput::StdErr { message } => (LogStream::Stderr, message),
            LogOutput::StdIn { .. } => return Vec::new(),
        };
        let frame = String::from_utf8_lossy(&bytes);
        let (time, mut rest) = Self::timestamp(&frame);
        let pending = match stream {
            LogStream::Stdout => &mut self.stdout,
            LogStream::Stderr => &mut self.stderr,
        };
        let mut lines = Vec::new();
        while let Some((line, more)) = rest.split_once('\n') {
            let start = pending.take().unwrap_or_else(|| PendingLine::new(time));
            lines.push(start.finish(stream, line));
            rest = more;
        }
        if !rest.is_empty() {
            pending
                .get_or_insert_with(|| PendingLine::new(time))
                .text
                .push_str(rest);
        }
        lines
    }

    /// The unfinished lines at the end of the log, stdout first.
    fn flush(&mut self) -> Vec<LogLine> {
        [
            (LogStream::Stdout, self.stdout.take()),
            (LogStream::Stderr, self.stderr.take()),
        ]
        .into_iter()
        .filter_map(|(stream, pending)| pending.map(|p| p.finish(stream, "")))
        .collect()
    }

    /// Splits the leading timestamp off a frame. A frame without one is all text.
    fn timestamp(frame: &str) -> (Option<SystemTime>, &str) {
        frame
            .split_once(' ')
            .and_then(|(stamp, rest)| {
                let time = OffsetDateTime::parse(stamp, &Rfc3339).ok()?;
                Some((Some(SystemTime::from(time)), rest))
            })
            .unwrap_or((None, frame))
    }
}

/// A registry credential in the shape podman's `X-Registry-Auth` header expects.
struct PullAuth<'a>(&'a str, &'a RegistryCredential);

impl From<PullAuth<'_>> for DockerCredentials {
    fn from(PullAuth(registry, credential): PullAuth<'_>) -> Self {
        Self {
            username: Some(credential.username().to_owned()),
            password: Some(credential.secret().to_owned()),
            serveraddress: Some(registry.to_owned()),
            ..Self::default()
        }
    }
}

/// A failed pull, as podman reported it: an HTTP status (when the request itself failed) and a
/// message (also used when the failure arrives mid-stream).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PullFailure {
    status: Option<u16>,
    message: String,
}

impl PullFailure {
    /// Removes the credential's secret from the message, in case a registry echoes it.
    fn redact(mut self, credential: Option<&RegistryCredential>) -> Self {
        if let Some(secret) = credential.map(RegistryCredential::secret) {
            self.message = self.message.replace(secret, "<redacted>");
        }
        self
    }

    /// Classifies the failure: refused access, a digest the registry does not have, or anything
    /// else. Podman answers a missing manifest with 404 and most other registry failures with
    /// 500 and the registry's words, so an auth or not-found status decides on its own and the
    /// message is only consulted otherwise. The phrases are narrow on purpose: a DNS failure
    /// ("no such host") is not a missing image, and a local "permission denied" is not the
    /// registry refusing access.
    fn into_error(self, image: &PinnedImage) -> RuntimeError {
        let text = self.message.to_ascii_lowercase();
        let any = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
        let (auth, missing) = match self.status {
            Some(401 | 403) => (true, false),
            Some(404) => (false, true),
            _ => (
                !text.contains("permission denied")
                    && any(&[
                        "unauthorized",
                        "authentication required",
                        "denied",
                        "forbidden",
                        "invalid username/password",
                    ]),
                any(&[
                    "manifest unknown",
                    "name unknown",
                    "manifest not found",
                    "404 not found",
                ]),
            ),
        };
        if auth {
            RuntimeError::RegistryAuth {
                registry: image.registry().to_owned(),
                image: image.to_string(),
                message: self.message,
            }
        } else if missing {
            RuntimeError::NotFound {
                kind: ObjectKind::Image,
                name: image.to_string(),
            }
        } else {
            RuntimeError::Api {
                operation: Operation::PullImage,
                message: format!("{image}: {}", self.message),
            }
        }
    }
}

impl From<BollardError> for PullFailure {
    fn from(err: BollardError) -> Self {
        match err {
            BollardError::DockerResponseServerError {
                status_code,
                message,
            } => Self {
                status: Some(status_code),
                message,
            },
            BollardError::DockerStreamError { error } => Self {
                status: None,
                message: error,
            },
            other => Self {
                status: None,
                message: other.to_string(),
            },
        }
    }
}

/// Against a real rootless podman; run with `TEST_HOSTER_PODMAN=1 cargo test`.
#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> bool {
        std::env::var("TEST_HOSTER_PODMAN").is_ok_and(|v| v == "1")
    }

    #[tokio::test]
    async fn missing_socket_names_the_path_and_doctor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("podman.sock");
        let err = PodmanRuntime::connect(&PodmanSocket::configured(&path))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RuntimeError::SocketMissing { path: p } if *p == path.display().to_string()),
            "{err:?}"
        );
        assert!(err.to_string().contains("hoster doctor"));
    }

    #[tokio::test]
    async fn discovery_never_falls_back_to_rootful_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let socket = PodmanSocket::discover(Some(dir.path()), None);
        let err = PodmanRuntime::connect(&socket).await.unwrap_err();
        let RuntimeError::SocketMissing { path } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(
            *path,
            dir.path().join("podman/podman.sock").display().to_string()
        );
        assert!(!err.to_string().contains("/run/podman"), "{err}");
        assert!(!err.to_string().contains("docker.sock"), "{err}");
    }

    #[test]
    fn discovery_follows_bollards_rootless_order() {
        let candidates = |dir: Option<&str>, uid| {
            PodmanSocket::discover(dir.map(Path::new), uid)
                .candidates()
                .to_vec()
        };
        assert_eq!(
            candidates(Some("/run/user/1000"), Some(1000)),
            [PathBuf::from("/run/user/1000/podman/podman.sock")]
        );
        assert_eq!(
            candidates(Some("/tmp/rt"), Some(1000)),
            [
                PathBuf::from("/tmp/rt/podman/podman.sock"),
                PathBuf::from("/run/user/1000/podman/podman.sock"),
            ]
        );
        assert_eq!(
            candidates(None, Some(42)),
            [PathBuf::from("/run/user/42/podman/podman.sock")]
        );
        assert_eq!(candidates(None, None).len(), 1);
        let socket = PodmanSocket::discover(Some(Path::new("/tmp/rt")), Some(7));
        assert_eq!(
            socket.to_string(),
            "/tmp/rt/podman/podman.sock or /run/user/7/podman/podman.sock"
        );
    }

    #[test]
    fn configured_socket_is_the_only_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("podman.sock");
        let socket = PodmanSocket::from_env(Some(&path));
        assert_eq!(socket.candidates(), std::slice::from_ref(&path));
        assert_eq!(socket.find(), None);
        std::fs::write(&path, "").unwrap();
        assert_eq!(socket.find(), Some(path.as_path()));
    }

    #[tokio::test]
    async fn connects_and_pings_real_podman() {
        if !enabled() {
            eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to run against rootless podman");
            return;
        }
        let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(PathBuf::from);
        let runtime = PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
            .await
            .expect("connect to rootless podman");
        // Negotiation must have brought the client down to what podman serves.
        let negotiated = runtime.docker.client_version();
        assert!(negotiated <= *API_DEFAULT_VERSION, "{negotiated}");
        runtime.docker.ping().await.expect("ping after negotiation");

        // The System page's runtime facts (ADR-0002: hoster only runs rootless).
        let engine = runtime.engine().await.expect("read podman's /info");
        assert!(engine.version.is_some(), "{engine:?}");
        assert_eq!(engine.rootless, Some(true), "{engine:?}");
        assert!(engine.cgroup_version.is_some(), "{engine:?}");
    }

    fn pinned() -> PinnedImage {
        format!("ghcr.io/acme/app@sha256:{}", "a".repeat(64))
            .parse()
            .unwrap()
    }

    fn failure(status: Option<u16>, message: &str) -> RuntimeError {
        PullFailure {
            status,
            message: message.to_owned(),
        }
        .into_error(&pinned())
    }

    #[test]
    fn pull_failures_are_classified() {
        let not_found = |err: &RuntimeError| matches!(err, RuntimeError::NotFound { kind: ObjectKind::Image, name } if *name == pinned().to_string());
        let auth = |err: &RuntimeError| matches!(err, RuntimeError::RegistryAuth { registry, .. } if registry == "ghcr.io");
        assert!(not_found(&failure(Some(404), "no such image")));
        assert!(not_found(&failure(
            None,
            "reading manifest sha256:aaa in ghcr.io/acme/app: manifest unknown"
        )));
        assert!(auth(&failure(Some(401), "anything")));
        assert!(auth(&failure(
            None,
            "initializing source docker://ghcr.io/acme/app@sha256:aaa: reading manifest: denied"
        )));
        assert!(auth(&failure(
            Some(500),
            "unable to retrieve auth token: invalid username/password: unauthorized"
        )));
        assert!(not_found(&failure(
            Some(500),
            "received unexpected HTTP status: 404 Not Found"
        )));
        assert!(auth(&failure(
            Some(500),
            "received unexpected HTTP status: 403 Forbidden"
        )));
        // Network and local failures are neither: they must not read as a missing digest or a
        // credential problem.
        for message in [
            "lookup ghcr.io: no such host",
            "connection refused",
            "open /home/hoster/.local/share/containers/storage/overlay: permission denied",
            "writing blob: no such file or directory",
        ] {
            let err = failure(Some(500), message);
            assert!(
                matches!(
                    err,
                    RuntimeError::Api {
                        operation: Operation::PullImage,
                        ..
                    }
                ),
                "{message}: {err:?}"
            );
        }
        assert!(matches!(
            failure(Some(500), "disk full"),
            RuntimeError::Api {
                operation: Operation::PullImage,
                ..
            }
        ));
        let message = failure(Some(401), "unauthorized").to_string();
        assert!(message.contains("ghcr.io"), "{message}");
    }

    #[test]
    fn pull_failures_never_carry_the_secret() {
        let credential = RegistryCredential::new("bot", "ghp_secret");
        let err = PullFailure {
            status: Some(401),
            message: "bad credentials ghp_secret".into(),
        }
        .redact(Some(&credential))
        .into_error(&pinned());
        assert!(!err.to_string().contains("ghp_secret"), "{err}");
        assert!(!format!("{err:?}").contains("ghp_secret"));
    }

    #[test]
    fn pull_auth_targets_the_registry_host() {
        let credential = RegistryCredential::new("bot", "ghp_secret");
        let auth = DockerCredentials::from(PullAuth("ghcr.io", &credential));
        assert_eq!(auth.username.as_deref(), Some("bot"));
        assert_eq!(auth.password.as_deref(), Some("ghp_secret"));
        assert_eq!(auth.serveraddress.as_deref(), Some("ghcr.io"));
    }

    /// A small public multi-arch image on ghcr, pinned by digest; override with
    /// `TEST_HOSTER_PULL_IMAGE`. No other test uses this tag: the orchestrator tests run
    /// containers from `busybox:1.36`, and an image in use cannot be removed.
    const PUBLIC_IMAGE: &str = "ghcr.io/containerd/busybox:1.32@sha256:ae39a6f5c07297d7ab64dbd4f82c77c874cc6a94cea29fdec309d0992574b4f7";

    async fn real_runtime() -> PodmanRuntime {
        let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(PathBuf::from);
        PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
            .await
            .expect("connect to rootless podman")
    }

    /// Needs rootless podman and network access to ghcr.io.
    #[tokio::test]
    async fn pulls_public_ghcr_image_by_digest() {
        if !enabled() {
            eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to pull from ghcr through podman");
            return;
        }
        let image: PinnedImage = std::env::var("TEST_HOSTER_PULL_IMAGE")
            .unwrap_or_else(|_| PUBLIC_IMAGE.to_owned())
            .parse()
            .expect("TEST_HOSTER_PULL_IMAGE must be pinned by digest");
        let runtime = real_runtime().await;
        let reference = image.to_string();
        // Start from a clean slate so the pull path really runs; absence is fine.
        match runtime.remove_image(&image).await {
            Ok(()) | Err(RuntimeError::NotFound { .. }) => {}
            Err(err) => panic!("cannot remove {reference}: {err}"),
        }
        assert!(!runtime.has_image(&reference).await.unwrap());
        runtime.pull_image(&image).await.expect("pull by digest");
        assert!(runtime.has_image(&reference).await.unwrap());
        // Present now: short-circuits without touching the registry.
        runtime.pull_image(&image).await.expect("second pull");
    }

    /// Needs rootless podman and network access to ghcr.io.
    #[tokio::test]
    async fn pulling_an_unknown_digest_is_not_found() {
        if !enabled() {
            eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to pull from ghcr through podman");
            return;
        }
        let image: PinnedImage = format!("ghcr.io/containerd/busybox@sha256:{}", "0".repeat(64))
            .parse()
            .unwrap();
        let err = real_runtime().await.pull_image(&image).await.unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeError::NotFound {
                    kind: ObjectKind::Image,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    use super::super::{ContainerFile, Labels, PodmanVersion, ServiceGeneration, VolumeMount};
    use crate::stack::Name;
    use crate::store::DeploymentId;

    /// The image the container lifecycle tests run (busybox has `httpd` and `sleep`), pinned
    /// by digest. Distinct from [`PUBLIC_IMAGE`] so the pull test can remove that one freely.
    const RUN_IMAGE: &str = "docker.io/library/busybox:1.37@sha256:66a6306db78bf2dbf3487f293aa8d6990d8e506fdffab9cc43fe422becf886e4";

    fn run_image() -> PinnedImage {
        RUN_IMAGE.parse().unwrap()
    }

    fn spec_for(stack: &str, service: &str) -> ContainerSpec {
        let generation = ServiceGeneration {
            stack: stack.parse().unwrap(),
            service: service.parse().unwrap(),
            generation: 1,
        };
        ContainerSpec::new(&generation, DeploymentId::from(1), run_image())
    }

    #[test]
    fn create_body_publishes_on_loopback_only_and_pins_safe_settings() {
        let mut spec = spec_for("notes", "web");
        spec.command = Some(vec!["httpd".into(), "-f".into()]);
        spec.entrypoint = Some(vec!["/bin/sh".into()]);
        spec.env.insert("FOO".into(), "bar=baz".into());
        spec.secrets
            .insert("DATABASE_URL".into(), "postgres://s3cret".into());
        assert!(
            !format!("{spec:?}").contains("s3cret"),
            "Debug leaks secrets"
        );
        spec.user = Some("1000:1000".into());
        spec.read_only = true;
        spec.stop_timeout = Duration::from_secs(25);
        spec.ports = vec![
            PortBinding {
                container_port: 8080,
                host_port: 20000,
            },
            PortBinding {
                container_port: 9000,
                host_port: 20001,
            },
        ];
        spec.volumes =
            vec![VolumeMount::named(&"notes".parse().unwrap(), "data", "/data").unwrap()];

        let body = ContainerCreateBody::from(&spec);
        assert_eq!(
            body.image.as_deref(),
            Some(
                "docker.io/library/busybox@sha256:66a6306db78bf2dbf3487f293aa8d6990d8e506fdffab9cc43fe422becf886e4"
            )
        );
        assert_eq!(body.cmd, spec.command);
        assert_eq!(body.entrypoint, spec.entrypoint);
        assert_eq!(
            body.env,
            Some(vec![
                "FOO=bar=baz".to_owned(),
                "DATABASE_URL=postgres://s3cret".to_owned()
            ])
        );
        assert_eq!(body.user.as_deref(), Some("1000:1000"));
        assert_eq!(body.stop_timeout, Some(25));
        assert_eq!(
            body.labels.unwrap().into_iter().collect::<Labels>(),
            spec.labels
        );
        let mut exposed = body.exposed_ports.unwrap();
        exposed.sort();
        assert_eq!(exposed, ["8080/tcp", "9000/tcp"]);

        let host = body.host_config.unwrap();
        let bindings = host.port_bindings.unwrap();
        assert_eq!(bindings.len(), 2);
        assert_eq!(
            bindings["8080/tcp"],
            Some(vec![bollard::models::PortBinding {
                host_ip: Some("127.0.0.1".into()),
                host_port: Some("20000".into()),
            }])
        );
        for binding in bindings.values().flatten().flatten() {
            assert_eq!(binding.host_ip.as_deref(), Some("127.0.0.1"));
        }
        assert_eq!(host.network_mode.as_deref(), Some("hoster-notes"));
        assert_eq!(host.publish_all_ports, Some(false));
        assert_eq!(
            host.restart_policy.unwrap().name,
            Some(RestartPolicyNameEnum::NO)
        );
        assert_eq!(host.privileged, Some(false));
        assert_eq!(host.readonly_rootfs, Some(true));
        assert!(host.binds.is_none());
        assert!(host.cap_add.is_none());
        assert!(host.devices.is_none());
        assert!(host.pid_mode.is_none());
        assert!(host.ipc_mode.is_none());
        assert!(host.volumes_from.is_none());
        let log = host.log_config.unwrap();
        assert_eq!(log.typ.as_deref(), Some("k8s-file"));
        assert_eq!(log.config.unwrap()["max-size"], "10m");
        let mounts = host.mounts.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].typ, Some(MountType::VOLUME));
        assert_eq!(mounts[0].source.as_deref(), Some("hoster-notes-data"));
        assert_eq!(mounts[0].target.as_deref(), Some("/data"));

        let endpoints = body.networking_config.unwrap().endpoints_config.unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(
            endpoints["hoster-notes"].aliases,
            Some(vec!["web".to_owned()])
        );
    }

    /// ADR-0059 §4: `shm_size`, `tmpfs` and `init` reach the create request only when set.
    #[test]
    fn create_body_sets_runtime_options_only_when_set() {
        let unset = ContainerCreateBody::from(&spec_for("notes", "web"))
            .host_config
            .unwrap();
        assert_eq!(unset.shm_size, None);
        assert_eq!(unset.tmpfs, None);
        assert_eq!(unset.init, None);

        let mut spec = spec_for("notes", "web");
        spec.shm_size = Some("256MiB".parse().unwrap());
        spec.tmpfs = std::collections::BTreeMap::from([
            ("/tmp".parse().unwrap(), "64MiB".parse().unwrap()),
            ("/var/cache/nginx".parse().unwrap(), "16MB".parse().unwrap()),
        ]);
        spec.init = true;
        let host = ContainerCreateBody::from(&spec).host_config.unwrap();
        assert_eq!(host.shm_size, Some(256 << 20));
        assert_eq!(
            host.tmpfs,
            Some(HashMap::from([
                (
                    "/tmp".to_owned(),
                    "rw,nosuid,nodev,mode=1777,size=67108864".to_owned()
                ),
                (
                    "/var/cache/nginx".to_owned(),
                    "rw,nosuid,nodev,mode=1777,size=16000000".to_owned()
                ),
            ]))
        );
        assert_eq!(host.init, Some(true));
        // ADR-0019 and architecture §8 hold with every option set.
        assert_eq!(host.privileged, Some(false));
        assert_eq!(host.publish_all_ports, Some(false));
        assert!(host.binds.is_none());
        assert!(host.cap_add.is_none());
        assert!(host.cap_drop.is_none());
        assert!(host.security_opt.is_none());
        assert!(host.sysctls.is_none());
        assert!(host.ulimits.is_none());
        assert!(host.devices.is_none());
        assert!(host.ipc_mode.is_none());
        assert!(host.pid_mode.is_none());
        assert!(host.uts_mode.is_none());
        assert!(host.userns_mode.is_none());
        assert!(host.oom_kill_disable.is_none());
        assert!(host.oom_score_adj.is_none());
        assert!(host.cpuset_cpus.is_none());
        assert!(host.memory.is_none());
        assert!(host.pids_limit.is_none());
        assert!(host.nano_cpus.is_none());

        // `init = false` is unset, not `Init: false`.
        spec.init = false;
        let host = ContainerCreateBody::from(&spec).host_config.unwrap();
        assert_eq!(host.init, None);
    }

    #[test]
    fn create_body_turns_podmans_own_healthcheck_off() {
        // Every container, whatever its spec: hoster runs all checks itself (ADR-0056).
        for spec in [spec_for("notes", "web"), spec_for("notes", "db")] {
            let health = ContainerCreateBody::from(&spec).healthcheck.unwrap();
            assert_eq!(health.test, Some(vec!["NONE".to_owned()]));
            assert_eq!(health.interval, None);
            assert_eq!(health.timeout, None);
            assert_eq!(health.retries, None);
            assert_eq!(health.start_period, None);
            assert_eq!(health.start_interval, None);
        }
    }

    #[test]
    fn exec_request_runs_argv_without_shell_stdin_or_tty() {
        let argv = vec!["pg_isready".to_owned(), "-U".to_owned(), "app".to_owned()];
        let config = ExecConfig::from(ExecRequest(Argv::try_from(&argv[..]).unwrap()));
        assert_eq!(config.cmd, Some(argv));
        assert_eq!(config.attach_stdin, Some(false));
        assert_eq!(config.attach_stdout, Some(true));
        assert_eq!(config.attach_stderr, Some(true));
        assert_eq!(config.tty, Some(false));
        // The container's user, environment and working directory.
        assert_eq!(config.user, None);
        assert_eq!(config.env, None);
        assert_eq!(config.working_dir, None);
        assert_eq!(config.privileged, None);
    }

    #[test]
    fn exec_on_a_stopped_container_is_not_running() {
        let server = |status_code, message: &str| BollardError::DockerResponseServerError {
            status_code,
            message: message.into(),
        };
        let call = Call::new(Operation::Exec, ObjectKind::Container, "c");
        let not_running = RuntimeError::NotRunning { name: "c".into() };
        // podman 6.1.4, verbatim.
        assert_eq!(
            call.error(server(
                500,
                "can only create exec sessions on running containers: container state improper"
            )),
            not_running
        );
        assert_eq!(
            call.error(server(409, "Container c is not running")),
            not_running
        );
        assert_eq!(
            call.error(server(404, "no container with name or ID \"c\" found")),
            RuntimeError::NotFound {
                kind: ObjectKind::Container,
                name: "c".into()
            }
        );
        // Other failures that merely mention "not running" are no container state.
        for message in ["boom", "conmon for exec session x is not running"] {
            assert!(
                matches!(
                    call.error(server(500, message)),
                    RuntimeError::Api {
                        operation: Operation::Exec,
                        ..
                    }
                ),
                "{message}"
            );
        }
    }

    fn exec_inspect(running: bool, exit_code: Option<i64>) -> ExecInspectResponse {
        ExecInspectResponse {
            running: Some(running),
            exit_code,
            ..ExecInspectResponse::default()
        }
    }

    #[test]
    fn exit_poll_waits_for_podman_to_settle() {
        let call = Call::new(Operation::Exec, ObjectKind::Container, "c");
        let start = Instant::now();
        let until = start + Duration::from_secs(1);
        let mut poll = ExitPoll::new(&call, "x", until);
        // Running for a moment after the output closed, then exited; the waits grow.
        let mut waits = Vec::new();
        for _ in 0..6 {
            let ControlFlow::Continue(wait) = poll.step(Ok(exec_inspect(true, None)), start) else {
                panic!("still running before the deadline");
            };
            waits.push(wait);
        }
        assert_eq!(
            waits,
            [20, 40, 80, 160, 250, 250].map(Duration::from_millis),
            "the waits double up to the cap"
        );
        assert_eq!(
            poll.step(Ok(exec_inspect(false, Some(3))), start),
            ControlFlow::Break(Ok(Some(3)))
        );

        // No wait reaches past the deadline, and still running at it is no exit code.
        let mut poll = ExitPoll::new(&call, "x", until);
        let late = until - Duration::from_millis(5);
        assert_eq!(
            poll.step(Ok(exec_inspect(true, None)), late),
            ControlFlow::Continue(Duration::from_millis(5))
        );
        assert_eq!(
            poll.step(Ok(exec_inspect(true, None)), until),
            ControlFlow::Break(Ok(None))
        );
    }

    #[test]
    fn exit_poll_reports_a_gone_session_as_not_running() {
        let call = Call::new(Operation::Exec, ObjectKind::Container, "c");
        let now = Instant::now();
        let mut poll = ExitPoll::new(&call, "x", now + Duration::from_secs(1));
        let gone = BollardError::DockerResponseServerError {
            status_code: 404,
            message: "no such exec session".into(),
        };
        assert_eq!(
            poll.step(Err(gone), now),
            ControlFlow::Break(Err(RuntimeError::NotRunning { name: "c".into() }))
        );
        assert!(matches!(
            poll.step(Ok(exec_inspect(false, None)), now),
            ControlFlow::Break(Err(RuntimeError::Api {
                operation: Operation::Exec,
                ..
            }))
        ));
        let broken = BollardError::DockerResponseServerError {
            status_code: 500,
            message: "boom".into(),
        };
        assert!(matches!(
            poll.step(Err(broken), now),
            ControlFlow::Break(Err(RuntimeError::Api { .. }))
        ));
    }

    /// A stand-in podman socket for session removal: it answers each connection with the next
    /// of `responses`, then with a 500, and records each request line.
    fn removal_socket(
        dir: &tempfile::TempDir,
        responses: &[&'static str],
    ) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
        let path = dir.path().join("podman.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut responses: std::collections::VecDeque<_> = responses.iter().copied().collect();
        tokio::spawn({
            let requests = Arc::clone(&requests);
            async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 512];
                    while !request.ends_with(b"{}") {
                        let read = stream.read(&mut buffer).await.unwrap();
                        assert_ne!(read, 0, "the request was cut short");
                        request.extend_from_slice(&buffer[..read]);
                    }
                    let line = String::from_utf8_lossy(&request)
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                    requests.lock().unwrap().push(line);
                    let response = responses
                        .pop_front()
                        .unwrap_or("HTTP/1.1 500 Internal Server Error\r\n\r\nbusy");
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            }
        });
        (path, requests)
    }

    const REMOVED: &str = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
    const BUSY: &str = "HTTP/1.1 500 Internal Server Error\r\n\r\nbusy";

    fn removal_of(exec: &str) -> String {
        format!("POST /v4.0.0/libpod/exec/{exec}/remove HTTP/1.1")
    }

    #[tokio::test]
    async fn a_failed_session_removal_is_retried_with_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, requests) = removal_socket(&dir, &[BUSY, REMOVED, REMOVED]);
        let sessions = ExecSessions::default();
        sessions.remove(&socket, "a".into()).await;
        assert_eq!(*sessions.pending.lock().unwrap(), ["a"]);
        sessions.remove(&socket, "b".into()).await;
        assert!(sessions.pending.lock().unwrap().is_empty());
        assert_eq!(
            *requests.lock().unwrap(),
            [removal_of("a"), removal_of("a"), removal_of("b")]
        );
    }

    #[tokio::test]
    async fn at_most_the_newest_failed_removals_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (socket, requests) = removal_socket(&dir, &[]);
        let sessions = ExecSessions::default();
        *sessions.pending.lock().unwrap() = (0..ExecSessions::MAX_PENDING)
            .map(|i| format!("s{i}"))
            .collect();
        sessions.remove(&socket, "new".into()).await;
        assert_eq!(
            requests.lock().unwrap().len(),
            ExecSessions::MAX_PENDING + 1
        );
        let pending = sessions.pending.lock().unwrap();
        assert_eq!(pending.len(), ExecSessions::MAX_PENDING);
        assert_eq!(pending.first().map(String::as_str), Some("s1"));
        assert_eq!(pending.last().map(String::as_str), Some("new"));
    }

    #[tokio::test]
    async fn a_podman_without_the_route_is_never_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let not_found = "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\n\r\nNot Found\n";
        let (socket, requests) = removal_socket(&dir, &[not_found]);
        let sessions = ExecSessions::default();
        sessions.remove(&socket, "a".into()).await;
        sessions.remove(&socket, "b".into()).await;
        assert_eq!(*requests.lock().unwrap(), [removal_of("a")]);
        assert!(sessions.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn session_removal_tells_gone_from_unsupported() {
        // podman 6.1.4's answers, verbatim (headers abridged).
        let removed = "HTTP/1.1 200 OK\r\nApi-Version: 1.44\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(SessionRemoval::from(removed), SessionRemoval::Removed);
        let gone = "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\r\n\
                    {\"cause\":\"no such exec session\",\"message\":\"no exec session with ID x \
                    found: no such exec session\",\"response\":404}";
        assert_eq!(SessionRemoval::from(gone), SessionRemoval::Gone);
        let missing_route = "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; \
                             charset=utf-8\r\nContent-Length: 10\r\n\r\nNot Found\n";
        assert_eq!(
            SessionRemoval::from(missing_route),
            SessionRemoval::Unsupported
        );
        let busy = "HTTP/1.1 500 Internal Server Error\r\n\r\n{\"cause\":\"busy\"}";
        assert!(
            matches!(SessionRemoval::from(busy), SessionRemoval::Failed(reason) if reason.contains("HTTP 500"))
        );
        assert!(matches!(
            SessionRemoval::from(""),
            SessionRemoval::Failed(_)
        ));
    }

    #[test]
    fn inspect_and_summary_convert_to_container_info() {
        let inspect: ContainerInspectResponse = serde_json::from_value(serde_json::json!({
            "Id": "abc",
            "Name": "/hoster-notes-web-g1",
            "State": {
                "Status": "exited",
                "ExitCode": 137,
                "OOMKilled": true,
                "StartedAt": "2026-11-08T13:00:00.5+01:00"
            },
            "Config": { "Labels": { "hoster.managed": "true" } },
            "NetworkSettings": { "Ports": {
                "8080/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "20000" }],
                "9000/tcp": [{ "HostIp": "", "HostPort": "20001" }],
                "9100/tcp": null
            } }
        }))
        .unwrap();
        let info = ContainerInfo::from(inspect);
        assert_eq!(info.id.as_ref(), "abc");
        assert_eq!(info.name, "hoster-notes-web-g1");
        assert_eq!(info.state, ContainerState::Exited(Some(137)));
        assert!(info.oom_killed);
        assert_eq!(
            info.started_at,
            Some(humantime::parse_rfc3339("2026-11-08T12:00:00.5Z").unwrap())
        );
        assert!(info.is_managed());
        assert_eq!(
            info.ports,
            [
                PublishedPort {
                    container_port: 8080,
                    host_ip: Ipv4Addr::LOCALHOST.into(),
                    host_port: 20000,
                },
                PublishedPort {
                    container_port: 9000,
                    host_ip: Ipv4Addr::UNSPECIFIED.into(),
                    host_port: 20001,
                },
            ]
        );

        let summary: ContainerSummary = serde_json::from_value(serde_json::json!({
            "Id": "def",
            "Names": ["hoster-notes-web-g2"],
            "State": "exited",
            "Status": "Exited (3) 2 seconds ago",
            "Labels": { "hoster.managed": "true" },
            "Ports": [{ "IP": "127.0.0.1", "PrivatePort": 8080, "PublicPort": 20002, "Type": "tcp" }]
        }))
        .unwrap();
        let info = ContainerInfo::from(summary);
        assert_eq!(info.name, "hoster-notes-web-g2");
        assert_eq!(info.state, ContainerState::Exited(Some(3)));
        assert_eq!(info.started_at, None, "the list does not say");
        assert_eq!(
            ContainerInfo::reported_time("0001-01-01T00:00:00Z"),
            None,
            "never started"
        );
        assert_eq!(ContainerInfo::reported_time("soon"), None);
        assert_eq!(
            info.ports,
            [PublishedPort {
                container_port: 8080,
                host_ip: Ipv4Addr::LOCALHOST.into(),
                host_port: 20002,
            }]
        );
        assert_eq!(
            ContainerState::reported("bogus", None),
            ContainerState::Dead
        );
        assert_eq!(
            ContainerState::reported("running", Some(0)),
            ContainerState::Running
        );
    }

    #[test]
    fn server_errors_become_typed_runtime_errors() {
        let server = |status_code, message: &str| BollardError::DockerResponseServerError {
            status_code,
            message: message.into(),
        };
        let mut spec = spec_for("notes", "web");
        spec.image = "ghcr.io/me/networkd:1@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".parse().unwrap();
        let create = Call::new(
            Operation::CreateContainer,
            ObjectKind::Container,
            &spec.name,
        );
        assert_eq!(
            create.create_error(&spec, server(409, "name is in use")),
            RuntimeError::Conflict {
                kind: ObjectKind::Container,
                name: "hoster-notes-web-g1".into()
            }
        );
        // podman 6.1's messages, verbatim. The image name contains "network" on purpose.
        assert_eq!(
            create.create_error(
                &spec,
                server(
                    404,
                    "no such image: ghcr.io/me/networkd@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb: image not known"
                )
            ),
            RuntimeError::NotFound {
                kind: ObjectKind::Image,
                name: "ghcr.io/me/networkd@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()
            }
        );
        assert_eq!(
            create.create_error(
                &spec,
                server(
                    500,
                    "container create: unable to find network with name or ID hoster-notes: \
                     network not found"
                )
            ),
            RuntimeError::NotFound {
                kind: ObjectKind::Network,
                name: "hoster-notes".into()
            }
        );
        assert!(matches!(
            create.create_error(&spec, server(500, "boom")),
            RuntimeError::Api {
                operation: Operation::CreateContainer,
                ..
            }
        ));
        let remove = Call::new(Operation::RemoveNetwork, ObjectKind::Network, "n");
        let in_use = RuntimeError::InUse {
            kind: ObjectKind::Network,
            name: "n".into(),
        };
        assert_eq!(remove.error(server(403, "active endpoints")), in_use);
        assert_eq!(remove.error(server(500, "network n is being used")), in_use);
        assert_eq!(
            remove.error(server(404, "no such network")),
            RuntimeError::NotFound {
                kind: ObjectKind::Network,
                name: "n".into()
            }
        );
        let start = Call::new(Operation::Start, ObjectKind::Container, "c");
        assert!(matches!(
            start.error(server(500, "boom")),
            RuntimeError::Api {
                operation: Operation::Start,
                ..
            }
        ));
    }

    /// A stack name no other test run uses, so parallel runs on one podman do not collide.
    fn unique_stack() -> Name {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("itest-{:x}-{nanos:x}", std::process::id())
            .parse()
            .unwrap()
    }

    async fn real_podman() -> Option<PodmanRuntime> {
        if !enabled() {
            eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to run against rootless podman");
            return None;
        }
        let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(PathBuf::from);
        Some(
            PodmanRuntime::connect(&PodmanSocket::from_env(socket.as_deref()))
                .await
                .expect("connect to rootless podman"),
        )
    }

    /// Removes every container, network and image labelled with `stack`.
    async fn clean_up(runtime: &PodmanRuntime, stack: &Name) {
        let filter = HashMap::from([("label", vec![format!("{}={stack}", label::STACK)])]);
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filter)
            .build();
        for container in runtime.docker.list_containers(Some(options)).await.unwrap() {
            let id = ContainerId::from(container.id.unwrap());
            let _ = runtime.remove(&id).await;
        }
        let options = bollard::query_parameters::ListNetworksOptionsBuilder::new()
            .filters(&filter)
            .build();
        for network in runtime.docker.list_networks(Some(options)).await.unwrap() {
            let _ = runtime.docker.remove_network(&network.name.unwrap()).await;
        }
        // Only images a test committed carry a test stack's label.
        let options = bollard::query_parameters::ListImagesOptionsBuilder::new()
            .filters(&filter)
            .build();
        for image in runtime.docker.list_images(Some(options)).await.unwrap() {
            let force = RemoveImageOptionsBuilder::new().force(true).build();
            let _ = runtime
                .docker
                .remove_image(&image.id, Some(force), None)
                .await;
        }
    }

    /// Listening TCP sockets on `port` in this network namespace, from /proc/net/tcp{,6}: the
    /// local address field in hex, e.g. `0100007F` for 127.0.0.1.
    fn listeners(port: u16) -> Vec<String> {
        let suffix = format!(":{port:04X}");
        ["/proc/net/tcp", "/proc/net/tcp6"]
            .iter()
            .flat_map(|path| {
                std::fs::read_to_string(path)
                    .unwrap_or_default()
                    .lines()
                    .skip(1)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let (local, state) = (fields.get(1)?, fields.get(3)?);
                (*state == "0A" && local.ends_with(&suffix))
                    .then(|| local.trim_end_matches(&suffix).to_owned())
            })
            .collect()
    }

    /// A blocking HTTP/1.0 GET; fine for a test, and keeps tokio's `net` feature out.
    fn http_get(port: u16) -> std::io::Result<String> {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        Ok(response)
    }

    /// Image GC relies on podman refusing to remove an image a container uses (ADR-0041).
    #[tokio::test]
    async fn removing_an_image_a_container_uses_is_in_use_on_real_podman() {
        let Some(runtime) = real_podman().await else {
            return;
        };
        let stack = unique_stack();
        let body = tokio::spawn({
            let runtime = runtime.clone();
            let stack = stack.clone();
            async move {
                let image = run_image();
                runtime.pull_image(&image).await.expect("pull busybox");
                runtime
                    .ensure_network(&NetworkSpec::for_stack(&stack))
                    .await
                    .unwrap();
                // Created, never started: a stopped container keeps its image in use too.
                runtime
                    .create_container(&spec_for(stack.as_ref(), "web"))
                    .await
                    .unwrap();
                // An image with one name is refused as in use. One that other references name
                // too (pulled by another digest or tag) only loses this name; either way the
                // container keeps its image.
                match runtime.remove_image(&image).await {
                    Err(RuntimeError::InUse { .. }) | Ok(()) => {}
                    Err(err) => panic!("{err:?}"),
                }
                assert!(runtime.has_image(&image.to_string()).await.unwrap());
            }
        });
        let result = body.await;
        clean_up(&runtime, &stack).await;
        if let Err(err) = result {
            std::panic::resume_unwind(err.into_panic());
        }
    }

    #[tokio::test]
    async fn network_and_container_lifecycle_on_real_podman() {
        let Some(runtime) = real_podman().await else {
            return;
        };
        let stack = unique_stack();
        // Run the body on its own task so a failed assertion still reaches the clean-up.
        let body = tokio::spawn({
            let runtime = runtime.clone();
            let stack = stack.clone();
            async move { lifecycle(&runtime, &stack).await }
        });
        let result = body.await;
        clean_up(&runtime, &stack).await;
        if let Err(err) = result {
            std::panic::resume_unwind(err.into_panic());
        }
    }

    async fn lifecycle(runtime: &PodmanRuntime, stack: &Name) {
        runtime
            .pull_image(&run_image())
            .await
            .expect("pull busybox");

        let network = NetworkSpec::for_stack(stack);
        runtime.ensure_network(&network).await.unwrap();
        runtime.ensure_network(&network).await.expect("idempotent");
        let inspected = runtime
            .docker
            .inspect_network(&network.name, None)
            .await
            .unwrap();
        let labels: Labels = inspected.labels.unwrap().into_iter().collect();
        assert_eq!(labels, network.labels);

        // A same-named network hoster did not create is not adopted.
        let foreign = format!("{}-foreign", network.name);
        runtime
            .docker
            .create_network(NetworkCreateRequest {
                name: foreign.clone(),
                labels: Some(HashMap::from([(label::STACK.into(), stack.to_string())])),
                ..NetworkCreateRequest::default()
            })
            .await
            .unwrap();
        let err = runtime
            .ensure_network(&NetworkSpec {
                name: foreign.clone(),
                labels: network.labels.clone(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Conflict { .. }), "{err:?}");

        let host_port = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut spec = spec_for(stack.as_ref(), "web");
        spec.entrypoint = Some(vec!["/bin/sh".into(), "-c".into()]);
        spec.command = Some(vec![
            "echo \"$GREETING\" > /tmp/index.html && exec httpd -f -p 8080 -h /tmp".into(),
        ]);
        spec.env
            .insert("GREETING".into(), "hello-from-hoster".into());
        spec.stop_timeout = Duration::from_secs(1);
        spec.ports = vec![PortBinding {
            container_port: 8080,
            host_port,
        }];
        // A missing image or network is told apart and named.
        let mut missing = spec.clone();
        missing.image = format!(
            "docker.io/library/hoster-no-such-image@sha256:{}",
            "0".repeat(64)
        )
        .parse()
        .unwrap();
        assert_eq!(
            runtime.create_container(&missing).await.unwrap_err(),
            RuntimeError::NotFound {
                kind: ObjectKind::Image,
                name: missing.image.to_string()
            }
        );
        let orphan = spec_for(&format!("{stack}-x"), "web");
        assert_eq!(
            runtime.create_container(&orphan).await.unwrap_err(),
            RuntimeError::NotFound {
                kind: ObjectKind::Network,
                name: orphan.network().to_owned()
            }
        );

        let id = runtime.create_container(&spec).await.unwrap();
        let err = runtime.create_container(&spec).await.unwrap_err();
        assert_eq!(
            err,
            RuntimeError::Conflict {
                kind: ObjectKind::Container,
                name: spec.name.clone()
            }
        );
        let created = runtime.inspect(&id).await.unwrap();
        assert_eq!(created.state, ContainerState::Created);
        assert_eq!(created.name, spec.name);
        assert_eq!(created.started_at, None, "never started");

        runtime.start(&id).await.unwrap();
        runtime.start(&id).await.expect("starting twice is a no-op");
        let info = runtime.inspect(&id).await.unwrap();
        assert_eq!(info.state, ContainerState::Running);
        let started = info.started_at.expect("a running container has started");
        assert!(
            started > SystemTime::now() - Duration::from_secs(600),
            "{started:?}"
        );
        for (key, value) in &spec.labels {
            assert_eq!(info.labels.get(key), Some(value), "label {key}");
        }
        assert_eq!(
            info.ports,
            [PublishedPort {
                container_port: 8080,
                host_ip: Ipv4Addr::LOCALHOST.into(),
                host_port,
            }]
        );

        // Raw inspect: what podman actually applied.
        let raw = runtime
            .docker
            .inspect_container(id.as_ref(), None)
            .await
            .unwrap();
        let host = raw.host_config.unwrap();
        assert_eq!(
            host.restart_policy.and_then(|p| p.name),
            Some(RestartPolicyNameEnum::NO)
        );
        assert_ne!(host.privileged, Some(true));
        let endpoint = &raw.network_settings.unwrap().networks.unwrap()[&network.name];
        assert!(
            endpoint
                .aliases
                .iter()
                .flatten()
                .any(|alias| alias == "web"),
            "{endpoint:?}"
        );

        // The published port answers on loopback ...
        let mut response = Err(std::io::Error::other("not tried"));
        for _ in 0..50 {
            response = http_get(host_port);
            if response
                .as_ref()
                .is_ok_and(|r| r.contains("hello-from-hoster"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let response = response.expect("GET from the published port");
        assert!(response.contains("hello-from-hoster"), "{response}");
        // ... and is bound on 127.0.0.1 only: no 0.0.0.0 or [::] listener (an `ss -ltn` check).
        let bound = listeners(host_port);
        assert_eq!(bound, ["0100007F"], "listeners on {host_port}: {bound:?}");

        let managed = runtime.list_managed().await.unwrap();
        let ours: Vec<_> = managed
            .iter()
            .filter(|c| c.labels.get(label::STACK) == Some(&stack.to_string()))
            .collect();
        assert_eq!(ours.len(), 1, "{managed:?}");
        assert_eq!(ours[0].id, id);
        assert_eq!(ours[0].state, ContainerState::Running);

        let err = runtime.remove_network(&network.name).await.unwrap_err();
        assert_eq!(
            err,
            RuntimeError::InUse {
                kind: ObjectKind::Network,
                name: network.name.clone()
            }
        );

        runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
        runtime
            .stop(&id, Duration::from_secs(1))
            .await
            .expect("stopping twice is a no-op");
        let stopped = runtime.inspect(&id).await.unwrap();
        assert!(
            matches!(stopped.state, ContainerState::Exited(Some(_))),
            "{stopped:?}"
        );
        assert!(listeners(host_port).is_empty());

        runtime.remove(&id).await.unwrap();
        assert!(matches!(
            runtime.inspect(&id).await,
            Err(RuntimeError::NotFound {
                kind: ObjectKind::Container,
                ..
            })
        ));
        runtime.remove_network(&network.name).await.unwrap();
        assert!(matches!(
            runtime.remove_network(&network.name).await,
            Err(RuntimeError::NotFound {
                kind: ObjectKind::Network,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn remove_kills_a_running_container() {
        let Some(runtime) = real_podman().await else {
            return;
        };
        let stack = unique_stack();
        let body = tokio::spawn({
            let runtime = runtime.clone();
            let stack = stack.clone();
            async move {
                runtime
                    .ensure_network(&NetworkSpec::for_stack(&stack))
                    .await
                    .unwrap();
                let mut spec = spec_for(stack.as_ref(), "sleeper");
                spec.command = Some(vec!["sleep".into(), "300".into()]);
                // The lifecycle test may not have run yet, so make sure the image is here.
                runtime
                    .pull_image(&run_image())
                    .await
                    .expect("pull busybox");
                let id = runtime.create_container(&spec).await.unwrap();
                runtime.start(&id).await.unwrap();
                runtime.remove(&id).await.expect("force remove");
                assert!(matches!(
                    runtime.remove(&id).await,
                    Err(RuntimeError::NotFound { .. })
                ));
            }
        });
        let result = body.await;
        clean_up(&runtime, &stack).await;
        if let Err(err) = result {
            std::panic::resume_unwind(err.into_panic());
        }
    }

    use crate::stack::{FileOwner, Ownership};

    /// A config file of the tests below, with `content` naming it.
    fn config_file(path: &str, mode: &str, owner: Ownership) -> ContainerFile {
        ContainerFile {
            path: path.parse().unwrap(),
            content: format!("content of {path}\n").as_str().into(),
            mode: mode.parse().unwrap(),
            owner,
        }
    }

    /// The three kinds of file: inline defaults, a numeric owner, and the container's user.
    fn config_files() -> Vec<ContainerFile> {
        vec![
            config_file(
                "/etc/app/conf.d/config.yml",
                "0444",
                Ownership::Ids(FileOwner::ROOT),
            ),
            config_file(
                "/etc/app/shared.key",
                "0440",
                Ownership::Ids(FileOwner { uid: 0, gid: 65534 }),
            ),
            config_file("/run/secrets/creds.json", "0400", Ownership::ContainerUser),
            // busybox links `/lib64` to `lib`.
            config_file(
                "/lib64/hoster/linked.conf",
                "0444",
                Ownership::Ids(FileOwner::ROOT),
            ),
            // An image file is replaced.
            config_file(
                "/etc/nsswitch.conf",
                "0444",
                Ownership::Ids(FileOwner::ROOT),
            ),
        ]
    }

    /// The combined output of `sh -c script` in the container, and its exit code.
    async fn sh(runtime: &PodmanRuntime, id: &ContainerId, script: &str) -> (i64, String) {
        let outcome = runtime
            .exec(id, &argv(&["sh", "-c", script]), Duration::from_secs(20))
            .await
            .unwrap();
        match outcome {
            ExecOutcome::Exited { code, output } => (code, output.as_ref().to_owned()),
            other => panic!("{script}: {other:?}"),
        }
    }

    /// Whether podman is new enough for config files (ADR-0057 §9); says so when it is not.
    fn uploads_supported(runtime: &PodmanRuntime) -> bool {
        let supported = PodmanVersion::CONFIG_FILES
            .require(&runtime.version(), "config files")
            .inspect_err(|reason| eprintln!("skipped: {reason}"));
        supported.is_ok()
    }

    /// ADR-0057 on real podman, for a container running as root and one running as another
    /// user with a read-only root: content, owner and mode as documented, parents root's,
    /// files kept over a restart and gone with the container.
    #[tokio::test]
    async fn config_files_reach_the_container_with_their_owner_and_mode() {
        with_stack(|runtime, stack| async move {
            if !uploads_supported(&runtime) {
                return;
            }
            // A container that runs throughout, so the stops below never leave the rootless
            // network namespace empty: tearing it down fails on GitHub's ubuntu-26.04 runner
            // ("rootless netns: kill network process: permission denied", Bears ghd), which
            // is not what this test is about. It also creates the network and pulls the image.
            running_sleeper(&runtime, &stack).await;
            assert_eq!(
                runtime.image_volumes(&run_image()).await.unwrap(),
                Vec::<String>::new()
            );
            let listing = "stat -c '%u:%g %a %n' /etc/app /etc/app/conf.d \
                           /etc/app/conf.d/config.yml /etc/app/shared.key /run/secrets \
                           /run/secrets/creds.json /lib/hoster /lib/hoster/linked.conf \
                           /etc/nsswitch.conf /etc /lib";

            // As root, writable root file system.
            let mut spec = spec_for(stack.as_ref(), "root");
            spec.command = Some(vec!["sleep".into(), "600".into()]);
            spec.stop_timeout = Duration::from_secs(1);
            spec.files = config_files();
            let id = runtime.create_container(&spec).await.unwrap();
            runtime.start(&id).await.unwrap();
            let (code, listed) = sh(&runtime, &id, listing).await;
            assert_eq!(code, 0, "{listed}");
            assert_eq!(
                listed.lines().collect::<Vec<_>>(),
                [
                    "0:0 755 /etc/app",
                    "0:0 755 /etc/app/conf.d",
                    "0:0 444 /etc/app/conf.d/config.yml",
                    "0:65534 440 /etc/app/shared.key",
                    "0:0 755 /run/secrets",
                    "0:0 400 /run/secrets/creds.json",
                    "0:0 755 /lib/hoster",
                    "0:0 444 /lib/hoster/linked.conf",
                    "0:0 444 /etc/nsswitch.conf",
                    // Existing directories keep their owner and mode.
                    "0:0 755 /etc",
                    "0:0 755 /lib",
                ]
            );
            let (_, content) = sh(
                &runtime,
                &id,
                "cat /etc/app/conf.d/config.yml /run/secrets/creds.json /lib64/hoster/linked.conf \
                 /etc/nsswitch.conf",
            )
            .await;
            assert_eq!(
                content,
                "content of /etc/app/conf.d/config.yml\ncontent of /run/secrets/creds.json\n\
                 content of /lib64/hoster/linked.conf\ncontent of /etc/nsswitch.conf\n"
            );
            // Files live in the container's layer: a restart keeps them.
            runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
            runtime.start(&id).await.unwrap();
            let (code, again) = sh(&runtime, &id, listing).await;
            assert_eq!((code, again), (0, listed));

            // As another user, with a read-only root file system.
            let mut spec = spec_for(stack.as_ref(), "user");
            spec.command = Some(vec!["sleep".into(), "600".into()]);
            spec.stop_timeout = Duration::from_secs(1);
            spec.user = Some("1000:1000".into());
            spec.read_only = true;
            spec.files = config_files();
            let id = runtime.create_container(&spec).await.unwrap();
            runtime.start(&id).await.unwrap();
            let (code, listed) = sh(&runtime, &id, listing).await;
            assert_eq!(code, 0, "{listed}");
            let listed: Vec<&str> = listed.lines().collect();
            assert_eq!(
                listed[..6],
                [
                    "0:0 755 /etc/app",
                    "0:0 755 /etc/app/conf.d",
                    "0:0 444 /etc/app/conf.d/config.yml",
                    "0:65534 440 /etc/app/shared.key",
                    // The parents of the user's file are still root's.
                    "0:0 755 /run/secrets",
                    "1000:1000 400 /run/secrets/creds.json",
                ]
            );
            // The user reads what the modes allow.
            let (code, content) = sh(
                &runtime,
                &id,
                "id -u; cat /etc/app/conf.d/config.yml /run/secrets/creds.json",
            )
            .await;
            assert_eq!(
                (code, content.as_str()),
                (
                    0,
                    "1000\ncontent of /etc/app/conf.d/config.yml\n\
                     content of /run/secrets/creds.json\n"
                )
            );
            let (code, _) = sh(&runtime, &id, "cat /etc/app/shared.key").await;
            assert_ne!(code, 0, "0440 for 0:65534 is not readable by 1000:1000");
            // And changes none of it, nor adds to it.
            for write in [
                "echo x >> /etc/app/conf.d/config.yml",
                "rm /etc/app/conf.d/config.yml",
                "echo x > /etc/app/new.conf",
                "echo x > /run/secrets/new.json",
                "rm /run/secrets/creds.json",
            ] {
                let (code, output) = sh(&runtime, &id, write).await;
                assert_ne!(code, 0, "{write}: {output}");
            }
            runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
            runtime.start(&id).await.unwrap();
            let (code, content) = sh(&runtime, &id, "cat /run/secrets/creds.json").await;
            assert_eq!(
                (code, content.as_str()),
                (0, "content of /run/secrets/creds.json\n"),
                "a read-only root refills /run from the layer at each start"
            );

            // Removing the container is the only cleanup there is.
            runtime.remove(&id).await.unwrap();
            assert!(matches!(
                runtime.inspect(&id).await,
                Err(RuntimeError::NotFound { .. })
            ));
        })
        .await;
    }

    /// A file that cannot be written fails the create, names container paths only and leaves
    /// no container behind.
    #[tokio::test]
    async fn a_refused_config_file_leaves_no_container() {
        with_stack(|runtime, stack| async move {
            if !uploads_supported(&runtime) {
                return;
            }
            runtime
                .ensure_network(&NetworkSpec::for_stack(&stack))
                .await
                .unwrap();
            runtime
                .pull_image(&run_image())
                .await
                .expect("pull busybox");
            let root = Ownership::Ids(FileOwner::ROOT);
            // No subuid range reaches this far.
            let far = Ownership::Ids(FileOwner {
                uid: 2_000_000_000,
                gid: 0,
            });
            let cases = [
                (
                    config_file("/etc/network", "0444", root),
                    "`/etc/network` is a directory in the image",
                ),
                (
                    config_file("/lib64", "0444", root),
                    "`/lib64` is a symbolic link in the image (to `/lib`)",
                ),
                (
                    config_file("/etc/passwd/x.conf", "0444", root),
                    "`/etc/passwd/x.conf` is below `/etc/passwd`, which is a file in the image",
                ),
                (
                    config_file("/etc/app/far.conf", "0444", far),
                    "error setting ownership of \"/etc/app/far.conf\" to 2000000000:0",
                ),
            ];
            for (i, (file, expected)) in cases.into_iter().enumerate() {
                let mut spec = spec_for(stack.as_ref(), &format!("refused{i}"));
                spec.command = Some(vec!["sleep".into(), "600".into()]);
                // A good file next to the bad one must not survive either.
                spec.files = vec![config_file("/etc/app/good.conf", "0444", root), file];
                let err = match runtime.create_container(&spec).await {
                    Err(err) => err,
                    Ok(_) => panic!("created with a file that must be refused: {expected}"),
                };
                let RuntimeError::ConfigFiles { message } = &err else {
                    panic!("{err:?}");
                };
                assert!(message.contains(expected), "{message}");
                assert!(
                    !message.contains("/home") && !message.contains("storage"),
                    "host paths in: {message}"
                );
                // By name, not by listing: other tests' containers come and go meanwhile.
                let left = runtime.inspect(&ContainerId::from(spec.name.clone())).await;
                assert!(
                    matches!(left, Err(RuntimeError::NotFound { .. })),
                    "{expected}: {left:?}"
                );
            }
        })
        .await;
    }

    /// A running busybox container of a fresh stack, for the exec tests.
    async fn running_sleeper(runtime: &PodmanRuntime, stack: &Name) -> ContainerId {
        runtime
            .ensure_network(&NetworkSpec::for_stack(stack))
            .await
            .unwrap();
        runtime
            .pull_image(&run_image())
            .await
            .expect("pull busybox");
        let mut spec = spec_for(stack.as_ref(), "sleeper");
        spec.command = Some(vec!["sleep".into(), "600".into()]);
        spec.stop_timeout = Duration::from_secs(1);
        let id = runtime.create_container(&spec).await.unwrap();
        runtime.start(&id).await.unwrap();
        id
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|&a| a.to_owned()).collect()
    }

    /// The exec sessions podman still keeps for a container, once the background removals
    /// have had a few seconds.
    async fn exec_sessions(runtime: &PodmanRuntime, id: &ContainerId) -> Vec<String> {
        let settled = Instant::now() + Duration::from_secs(5);
        loop {
            let sessions = runtime
                .docker
                .inspect_container(id.as_ref(), None)
                .await
                .unwrap()
                .exec_ids
                .unwrap_or_default();
            if sessions.is_empty() || Instant::now() >= settled {
                return sessions;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Runs `body` against a fresh stack on real podman and cleans the stack up afterwards,
    /// also when `body` panics.
    async fn with_stack<F, Fut>(body: F)
    where
        F: FnOnce(PodmanRuntime, Name) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let Some(runtime) = real_podman().await else {
            return;
        };
        let stack = unique_stack();
        let result = tokio::spawn(body(runtime.clone(), stack.clone())).await;
        clean_up(&runtime, &stack).await;
        if let Err(err) = result {
            std::panic::resume_unwind(err.into_panic());
        }
    }

    #[tokio::test]
    async fn exec_reports_exit_codes_output_and_cannot_run() {
        with_stack(|runtime, stack| async move {
            let id = running_sleeper(&runtime, &stack).await;
            let timeout = Duration::from_secs(20);

            let passed = runtime.exec(&id, &argv(&["true"]), timeout).await.unwrap();
            assert!(passed.passed(), "{passed:?}");

            let printed = runtime
                .exec(
                    &id,
                    &argv(&["sh", "-c", "echo out; echo err >&2; exit 3"]),
                    timeout,
                )
                .await
                .unwrap();
            let ExecOutcome::Exited { code: 3, output } = &printed else {
                panic!("{printed:?}");
            };
            assert!(output.as_ref().contains("out\n"), "{output}");
            assert!(output.as_ref().contains("err\n"), "{output}");

            // Runs as the container's user, with its environment, without a shell.
            let env = runtime
                .exec(
                    &id,
                    &argv(&["sh", "-c", "id -u; echo \"$PATH\"; echo $0"]),
                    timeout,
                )
                .await
                .unwrap();
            assert!(env.passed(), "{env:?}");
            assert!(env.output().as_ref().starts_with("0\n/"), "{env:?}");

            for (args, needle) in [
                (&["pg_isready"][..], "executable file not found"),
                (&["/etc/passwd"][..], "permission denied"),
                (&["/etc"][..], "is a directory"),
            ] {
                let outcome = runtime.exec(&id, &argv(args), timeout).await.unwrap();
                let ExecOutcome::CannotRun { reason, output, .. } = &outcome else {
                    panic!("{args:?}: {outcome:?}");
                };
                assert!(reason.contains(needle), "{args:?}: {reason}");
                assert!(!output.as_ref().contains("unixpacket"), "{output}");
            }
            // A shell's 127 is the command's own failure.
            let shell = runtime
                .exec(&id, &argv(&["sh", "-c", "pg_isready"]), timeout)
                .await
                .unwrap();
            assert!(
                matches!(shell, ExecOutcome::Exited { code: 127, .. }),
                "{shell:?}"
            );

            // Every finished session was removed.
            assert_eq!(exec_sessions(&runtime, &id).await, Vec::<String>::new());
        })
        .await;
    }

    #[tokio::test]
    async fn a_timed_out_exec_is_killed_with_its_group() {
        with_stack(|runtime, stack| async move {
            let id = running_sleeper(&runtime, &stack).await;
            let started = Instant::now();
            let outcome = runtime
                .exec(
                    &id,
                    &argv(&["sh", "-c", "echo begun; sleep 300 & sleep 301"]),
                    Duration::from_secs(1),
                )
                .await
                .unwrap();
            let elapsed = started.elapsed();
            assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
            let ExecOutcome::TimedOut { output, exec } = outcome else {
                panic!("{outcome:?}");
            };
            assert_eq!(output.as_ref(), "begun\n");
            tokio::time::timeout(Duration::from_secs(15), exec.ended())
                .await
                .expect("the exec is killed");
            // Neither the shell nor the backgrounded sleep survived it.
            let left = runtime
                .exec(
                    &id,
                    &argv(&["sh", "-c", "ps -o args | grep '^sleep 30[01]'"]),
                    Duration::from_secs(10),
                )
                .await
                .unwrap();
            assert!(
                matches!(left, ExecOutcome::Exited { code: 1, .. }),
                "{left:?}"
            );
            assert_eq!(exec_sessions(&runtime, &id).await, Vec::<String>::new());
        })
        .await;
    }

    #[tokio::test]
    async fn an_exec_cut_off_at_any_point_is_ended_and_removed() {
        with_stack(|runtime, stack| async move {
            let id = running_sleeper(&runtime, &stack).await;
            // From before the create answers to while the command runs.
            for millis in [0, 5, 20, 50, 100, 300] {
                let outcome = runtime
                    .exec(&id, &argv(&["sleep", "30"]), Duration::from_millis(millis))
                    .await
                    .unwrap();
                let ExecOutcome::TimedOut { exec, .. } = outcome else {
                    panic!("{millis} ms: {outcome:?}");
                };
                tokio::time::timeout(Duration::from_secs(15), exec.ended())
                    .await
                    .unwrap_or_else(|_| panic!("{millis} ms: the exec is never ended"));
            }
            let left = runtime
                .exec(
                    &id,
                    &argv(&["sh", "-c", "ps -o args | grep '^sleep 30$'"]),
                    Duration::from_secs(10),
                )
                .await
                .unwrap();
            assert!(
                matches!(left, ExecOutcome::Exited { code: 1, .. }),
                "{left:?}"
            );
            assert_eq!(exec_sessions(&runtime, &id).await, Vec::<String>::new());
        })
        .await;
    }

    #[tokio::test]
    async fn exec_needs_a_running_container() {
        with_stack(|runtime, stack| async move {
            let id = running_sleeper(&runtime, &stack).await;
            let timeout = Duration::from_secs(20);
            // The container stops while the command runs.
            let stopper = tokio::spawn({
                let runtime = runtime.clone();
                let id = id.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
                }
            });
            let err = runtime
                .exec(&id, &argv(&["sleep", "30"]), timeout)
                .await
                .unwrap_err();
            stopper.await.unwrap();
            assert_eq!(
                err,
                RuntimeError::NotRunning {
                    name: id.to_string()
                }
            );
            // Stopped: refused before anything runs.
            let err = runtime
                .exec(&id, &argv(&["true"]), timeout)
                .await
                .unwrap_err();
            assert_eq!(
                err,
                RuntimeError::NotRunning {
                    name: id.to_string()
                }
            );
            runtime.remove(&id).await.unwrap();
            assert!(matches!(
                runtime.exec(&id, &argv(&["true"]), timeout).await,
                Err(RuntimeError::NotFound {
                    kind: ObjectKind::Container,
                    ..
                })
            ));
        })
        .await;
    }

    /// ADR-0059 on real podman (architecture §1.8): `/dev/shm` has `shm_size`, a tmpfs on a
    /// directory the image has (root's, mode 755) gets its size and mode 1777 and a non-root
    /// user can write to it, and `init` makes PID 1 podman's init. `read_only` keeps podman's
    /// own tmpfs on `/run` and `/var/tmp`.
    #[tokio::test]
    async fn runtime_options_reach_the_container() {
        with_stack(|runtime, stack| async move {
            running_sleeper(&runtime, &stack).await;
            let mut spec = spec_for(stack.as_ref(), "options");
            spec.command = Some(argv(&["sleep", "600"]));
            spec.stop_timeout = Duration::from_secs(1);
            spec.user = Some("1000:1000".into());
            spec.read_only = true;
            spec.shm_size = Some("256MiB".parse().unwrap());
            spec.tmpfs = std::collections::BTreeMap::from([(
                "/home".parse().unwrap(),
                "16MiB".parse().unwrap(),
            )]);
            spec.init = true;
            let id = runtime.create_container(&spec).await.unwrap();
            runtime.start(&id).await.unwrap();

            let (code, output) = sh(
                &runtime,
                &id,
                "df -k /dev/shm /home | tail -n 2 | awk '{print $6, $2}'; stat -c '%a %U' /home; \
                 touch /home/written && echo written; tr '\\0' ' ' < /proc/1/cmdline; echo; \
                 grep -E ' /(home|run|var/tmp) ' /proc/mounts | awk '{print \"mount\", $2, $4}'",
            )
            .await;
            assert_eq!(code, 0, "{output}");
            let lines: Vec<&str> = output.lines().collect();
            assert_eq!(
                lines[..4],
                ["/dev/shm 262144", "/home 16384", "1777 root", "written"]
            );
            assert!(
                lines[4].starts_with("/run/podman-init -- sleep 600"),
                "PID 1 is {:?}",
                lines[4]
            );
            let mount = |path: &str| {
                let prefix = format!("mount {path} ");
                *lines
                    .iter()
                    .find(|l| l.starts_with(&prefix))
                    .unwrap_or_else(|| panic!("{path} is not a mount: {output}"))
            };
            let home = mount("/home");
            assert!(home.contains("nosuid") && home.contains("nodev"), "{home}");
            assert!(!home.contains("noexec"), "{home}");
            assert!(home.contains("size=16384k"), "{home}");
            // podman's own, unsized: only the declared path is replaced.
            for own in ["/run", "/var/tmp"] {
                assert!(!mount(own).contains("size="), "{}", mount(own));
            }
        })
        .await;
    }

    /// A docker-format image that declares a HEALTHCHECK must not get podman's timer check in a
    /// container hoster created; one created without `Healthcheck NONE` does, which shows the
    /// image's check is real.
    #[tokio::test]
    async fn podman_never_runs_an_images_healthcheck() {
        use bollard::models::ContainerConfig;
        use bollard::query_parameters::CommitContainerOptionsBuilder;

        with_stack(|runtime, stack| async move {
            let id = running_sleeper(&runtime, &stack).await;
            runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
            // Compat commit writes a docker-format image, which keeps the HEALTHCHECK.
            let repository = format!("localhost/{stack}-healthcheck");
            let options = CommitContainerOptionsBuilder::new()
                .container(id.as_ref())
                .repo(&repository)
                .tag("latest")
                .build();
            let config = ContainerConfig {
                cmd: Some(argv(&["sleep", "600"])),
                healthcheck: Some(HealthConfig {
                    test: Some(argv(&["CMD-SHELL", "touch /tmp/image-check-ran"])),
                    interval: Some(1_000_000_000),
                    timeout: Some(1_000_000_000),
                    retries: Some(1),
                    ..HealthConfig::default()
                }),
                labels: Some(HashMap::from([(label::STACK.into(), stack.to_string())])),
                ..ContainerConfig::default()
            };
            runtime
                .docker
                .commit_container(options, config)
                .await
                .expect("commit an image with a HEALTHCHECK");
            let tagged = format!("{repository}:latest");
            let image = runtime.docker.inspect_image(&tagged).await.unwrap();
            let digest = image
                .repo_digests
                .iter()
                .flatten()
                .find_map(|d| d.split_once('@').map(|(_, digest)| digest.to_owned()))
                .expect("a committed image has a digest");
            {
                let pinned: PinnedImage = format!("{repository}@{digest}").parse().unwrap();
                let mut spec = spec_for(stack.as_ref(), "checked");
                spec.image = pinned;
                spec.stop_timeout = Duration::from_secs(1);
                let ours = runtime.create_container(&spec).await.unwrap();
                runtime.start(&ours).await.unwrap();

                let mut control = ContainerCreateBody::from(&spec_for(stack.as_ref(), "control"));
                control.image = Some(tagged.clone());
                control.cmd = None;
                control.healthcheck = None;
                let name = format!("hoster-{stack}-control-g1");
                let created = runtime
                    .docker
                    .create_container(
                        Some(CreateContainerOptionsBuilder::new().name(&name).build()),
                        control,
                    )
                    .await
                    .unwrap();
                let control = ContainerId::from(created.id);
                runtime.start(&control).await.unwrap();

                let ran = argv(&["test", "-e", "/tmp/image-check-ran"]);
                let timeout = Duration::from_secs(10);
                let mut control_ran = false;
                for _ in 0..50 {
                    if runtime
                        .exec(&control, &ran, timeout)
                        .await
                        .unwrap()
                        .passed()
                    {
                        control_ran = true;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                assert!(control_ran, "podman ran the image's check without NONE");
                let ours_ran = runtime.exec(&ours, &ran, timeout).await.unwrap();
                assert!(
                    matches!(ours_ran, ExecOutcome::Exited { code: 1, .. }),
                    "podman ran the image's check: {ours_ran:?}"
                );
                let raw = runtime
                    .docker
                    .inspect_container(ours.as_ref(), None)
                    .await
                    .unwrap();
                assert_eq!(
                    raw.config.and_then(|c| c.healthcheck).and_then(|h| h.test),
                    Some(argv(&["NONE"]))
                );
                assert_eq!(raw.state.and_then(|s| s.health), None);
                runtime.remove(&ours).await.unwrap();
                runtime.remove(&control).await.unwrap();
            }
            // `clean_up` removes the image: it carries the stack label of the container it
            // was committed from.
        })
        .await;
    }

    fn stdout(text: &str) -> LogOutput {
        LogOutput::StdOut {
            message: text.to_owned().into(),
        }
    }

    fn stderr(text: &str) -> LogOutput {
        LogOutput::StdErr {
            message: text.to_owned().into(),
        }
    }

    fn rfc3339(s: &str) -> SystemTime {
        humantime::parse_rfc3339(s).unwrap()
    }

    #[test]
    fn log_frames_become_whole_lines_with_utc_times() {
        let mut log = ContainerLog::default();
        // podman 6.1 stamps frames in the host's offset.
        let lines = log.push(stdout("2026-11-08T00:29:20.630992353+01:00 out1\n"));
        assert_eq!(
            lines,
            [LogLine {
                stream: LogStream::Stdout,
                time: Some(rfc3339("2026-11-07T23:29:20.630992353Z")),
                text: "out1".into(),
            }]
        );
        // A partial frame waits for its newline; the other stream is not held up by it.
        assert!(
            log.push(stdout("2026-11-08T00:29:20.630992353+01:00 part"))
                .is_empty()
        );
        let err = log.push(stderr("2026-11-08T00:29:21Z err1\r\n"));
        assert_eq!(err[0].stream, LogStream::Stderr);
        assert_eq!(err[0].text, "err1");
        assert_eq!(err[0].time, Some(rfc3339("2026-11-08T00:29:21Z")));
        assert!(log.push(stdout("2026-11-08T00:29:22+01:00 ial")).is_empty());
        let joined = log.push(stdout("2026-11-08T00:29:23+01:00 \nnext\n"));
        assert_eq!(joined.len(), 2);
        assert_eq!(joined[0].text, "partial");
        assert_eq!(
            joined[0].time,
            Some(rfc3339("2026-11-07T23:29:20.630992353Z")),
            "a joined line keeps the time of its first frame"
        );
        assert_eq!(joined[1].text, "next");
        // An empty line is a line.
        assert_eq!(log.push(stdout("2026-11-08T00:29:24Z \n"))[0].text, "");
        // Without a timestamp, the whole frame is text.
        let plain = log.push(stdout("no-stamp here\n"));
        assert_eq!(
            (plain[0].time, plain[0].text.as_str()),
            (None, "no-stamp here")
        );
        // stdin is never shown; a TTY counts as stdout.
        assert!(
            log.push(LogOutput::StdIn {
                message: "x\n".into()
            })
            .is_empty()
        );
        let console = log.push(LogOutput::Console {
            message: "tty\n".into(),
        });
        assert_eq!(console[0].stream, LogStream::Stdout);

        // The end of the log flushes unfinished lines.
        assert!(log.push(stderr("2026-11-08T00:29:25Z tail")).is_empty());
        let rest = log.flush();
        assert_eq!(rest.len(), 1);
        assert_eq!(
            (rest[0].stream, rest[0].text.as_str()),
            (LogStream::Stderr, "tail")
        );
        assert!(log.flush().is_empty());
    }

    #[test]
    fn log_query_asks_for_both_streams_with_timestamps() {
        let query = ContainerLog::query(LogOptions::default());
        assert!(query.stdout && query.stderr && query.timestamps);
        assert!(!query.follow);
        assert_eq!(query.tail, "all");
        assert_eq!(query.since, 0);

        let query = ContainerLog::query(LogOptions {
            follow: true,
            tail: Some(50),
            since: Some(UNIX_EPOCH + Duration::from_millis(1_700_000_000_900)),
        });
        assert!(query.follow);
        assert_eq!(query.tail, "50");
        assert_eq!(query.since, 1_700_000_000, "rounded down to the second");
    }

    #[tokio::test]
    async fn streams_logs_from_a_real_container() {
        let Some(runtime) = real_podman().await else {
            return;
        };
        let stack = unique_stack();
        let body = tokio::spawn({
            let runtime = runtime.clone();
            let stack = stack.clone();
            async move { stream_logs(&runtime, &stack).await }
        });
        let result = body.await;
        clean_up(&runtime, &stack).await;
        if let Err(err) = result {
            std::panic::resume_unwind(err.into_panic());
        }
    }

    async fn stream_logs(runtime: &PodmanRuntime, stack: &Name) {
        use futures_util::TryStreamExt;

        runtime
            .ensure_network(&NetworkSpec::for_stack(stack))
            .await
            .unwrap();
        runtime
            .pull_image(&run_image())
            .await
            .expect("pull busybox");
        let mut spec = spec_for(stack.as_ref(), "talker");
        spec.command = Some(vec![
            "sh".into(),
            "-c".into(),
            "echo out1; echo err1 >&2; printf part; sleep 1; echo ial; \
             while sleep 0.2; do echo tick; done"
                .into(),
        ]);
        let id = runtime.create_container(&spec).await.unwrap();
        let before = SystemTime::now() - Duration::from_secs(2);
        runtime.start(&id).await.unwrap();

        // Followed: the lines arrive live, whole and on their own streams.
        let follow = LogOptions {
            follow: true,
            ..LogOptions::default()
        };
        let mut lines = runtime.logs(&id, follow);
        let mut seen = Vec::new();
        tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(line) = lines.next().await {
                let line = line.expect("log line");
                let tick = line.text == "tick";
                seen.push(line);
                if tick {
                    break;
                }
            }
        })
        .await
        .expect("the followed log yields the first tick");
        drop(lines);
        let find = |text: &str| {
            seen.iter()
                .find(|line| line.text == text)
                .unwrap_or_else(|| panic!("`{text}` in {seen:?}"))
        };
        assert_eq!(find("out1").stream, LogStream::Stdout);
        assert_eq!(find("err1").stream, LogStream::Stderr);
        assert_eq!(find("partial").stream, LogStream::Stdout);
        for line in &seen {
            let time = line.time.expect("podman stamps every line");
            assert!(time >= before, "{line:?} is not from before the start");
        }

        // Not followed: the stream ends, and `tail` keeps the last lines only.
        let tail: Vec<_> = tokio::time::timeout(
            Duration::from_secs(30),
            runtime
                .logs(
                    &id,
                    LogOptions {
                        tail: Some(1),
                        ..LogOptions::default()
                    },
                )
                .try_collect(),
        )
        .await
        .expect("an unfollowed log ends")
        .unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].text, "tick");

        // `since` in the future leaves nothing.
        let none: Vec<_> = runtime
            .logs(
                &id,
                LogOptions {
                    since: Some(SystemTime::now() + Duration::from_secs(3600)),
                    ..LogOptions::default()
                },
            )
            .try_collect()
            .await
            .unwrap();
        assert!(none.is_empty(), "{none:?}");

        let missing = ContainerId::from("hoster-no-such-container".to_owned());
        let result: Result<Vec<_>, _> = runtime
            .logs(&missing, LogOptions::default())
            .try_collect()
            .await;
        assert!(
            matches!(result, Err(RuntimeError::NotFound { .. })),
            "{result:?}"
        );
        runtime.remove(&id).await.unwrap();
    }
}
