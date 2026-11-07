//! [`PodmanRuntime`]: the rootless podman socket through bollard's Docker-compatible client
//! (ADR-0002, docs/architecture.md §1.3).

use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bollard::errors::Error as BollardError;
use bollard::{API_DEFAULT_VERSION, Docker};
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};

use super::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerSpec, LogLine, LogOptions, NetworkSpec,
    Operation, RuntimeError,
};
use crate::stack::ImageRef;

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
    reason = "used by the network, container, pull and log operations as they are implemented"
)]
pub struct PodmanRuntime {
    docker: Docker,
    /// The socket connected to.
    socket: PathBuf,
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
        })
    }

    /// A placeholder for the operations implemented by later tasks.
    fn unsupported(operation: Operation) -> RuntimeError {
        RuntimeError::Unsupported { operation }
    }
}

impl ContainerRuntime for PodmanRuntime {
    async fn ensure_network(&self, _network: &NetworkSpec) -> Result<(), RuntimeError> {
        Err(Self::unsupported(Operation::EnsureNetwork))
    }

    async fn remove_network(&self, _name: &str) -> Result<(), RuntimeError> {
        Err(Self::unsupported(Operation::RemoveNetwork))
    }

    async fn pull_image(&self, _image: &ImageRef) -> Result<(), RuntimeError> {
        Err(Self::unsupported(Operation::PullImage))
    }

    async fn create_container(&self, _spec: &ContainerSpec) -> Result<ContainerId, RuntimeError> {
        Err(Self::unsupported(Operation::CreateContainer))
    }

    async fn start(&self, _id: &ContainerId) -> Result<(), RuntimeError> {
        Err(Self::unsupported(Operation::Start))
    }

    async fn stop(&self, _id: &ContainerId, _timeout: Duration) -> Result<(), RuntimeError> {
        Err(Self::unsupported(Operation::Stop))
    }

    async fn remove(&self, _id: &ContainerId) -> Result<(), RuntimeError> {
        Err(Self::unsupported(Operation::Remove))
    }

    async fn inspect(&self, _id: &ContainerId) -> Result<ContainerInfo, RuntimeError> {
        Err(Self::unsupported(Operation::Inspect))
    }

    async fn list_managed(&self) -> Result<Vec<ContainerInfo>, RuntimeError> {
        Err(Self::unsupported(Operation::ListManaged))
    }

    fn logs(
        &self,
        _id: &ContainerId,
        _options: LogOptions,
    ) -> BoxStream<'static, Result<LogLine, RuntimeError>> {
        stream::once(async { Err(Self::unsupported(Operation::Logs)) }).boxed()
    }
}

/// Against a real rootless podman; run with `HOSTER_PODMAN_TESTS=1 cargo test`.
#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> bool {
        std::env::var("HOSTER_PODMAN_TESTS").is_ok_and(|v| v == "1")
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
            eprintln!("skipped: set HOSTER_PODMAN_TESTS=1 to run against rootless podman");
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
    }
}
