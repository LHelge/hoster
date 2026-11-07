//! [`PodmanRuntime`]: the rootless podman socket through bollard's Docker-compatible client
//! (ADR-0002, docs/architecture.md §1.3).

use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bollard::auth::DockerCredentials;
use bollard::errors::Error as BollardError;
use bollard::query_parameters::CreateImageOptionsBuilder;
use bollard::{API_DEFAULT_VERSION, Docker};
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};

use super::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerSpec, LogLine, LogOptions, NetworkSpec,
    ObjectKind, Operation, RuntimeError,
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
    reason = "used by the network, container, pull and log operations as they are implemented"
)]
pub struct PodmanRuntime {
    docker: Docker,
    /// The socket connected to.
    socket: PathBuf,
    /// Pull credentials per registry host (ADR-0025); anonymous unless set.
    credentials: Arc<dyn RegistryCredentials>,
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
            credentials: Arc::new(Anonymous),
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
    /// `HOSTER_PULL_TEST_IMAGE`.
    const PUBLIC_IMAGE: &str = "ghcr.io/containerd/busybox:1.36@sha256:7b3ccabffc97de872a30dfd234fd972a66d247c8cfc69b0550f276481852627c";

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
            eprintln!("skipped: set HOSTER_PODMAN_TESTS=1 to pull from ghcr through podman");
            return;
        }
        let image: PinnedImage = std::env::var("HOSTER_PULL_TEST_IMAGE")
            .unwrap_or_else(|_| PUBLIC_IMAGE.to_owned())
            .parse()
            .expect("HOSTER_PULL_TEST_IMAGE must be pinned by digest");
        let runtime = real_runtime().await;
        let reference = image.to_string();
        // Start from a clean slate so the pull path really runs; absence is fine.
        let _ = runtime
            .docker
            .remove_image(
                &reference,
                None::<bollard::query_parameters::RemoveImageOptions>,
                None,
            )
            .await;
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
            eprintln!("skipped: set HOSTER_PODMAN_TESTS=1 to pull from ghcr through podman");
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
}
