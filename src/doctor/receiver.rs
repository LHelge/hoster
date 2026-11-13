//! The doctor's `otlp_path` check (ADR-0069 §3): a container on a stack network resolves
//! `host.containers.internal` to `169.254.1.2` and reaches the OTLP receiver through the
//! proxy's `:80`.
//!
//! [`ReceiverPath`] runs a throwaway busybox container through the same runtime calls a deploy
//! makes ([`ContainerRuntime`]), on an existing stack network (or a temporary one when no stack
//! has a network yet), and has it `POST /v1/traces` without a token. busybox `wget` prints the
//! address it connects to and the status it got: `401` is the receiver's answer, so the whole
//! path works; `404` is the proxy's unknown-host page, so the peer check failed; no status means
//! nothing answered (hoster is not running). The container carries no `hoster.managed` label,
//! so a running hoster ignores it, and it is removed again whatever happens.
//!
//! The busybox image is pulled from Docker Hub by digest when it is not present. A pull that
//! fails makes the check a warning, like every other way it can fail: the check is advisory.

use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use crate::otlp::OtlpReceiver;
use crate::runtime::{
    ContainerRuntime, ContainerSpec, NetworkSpec, PodmanRuntime, PodmanSocket, ServiceGeneration,
    label,
};
use crate::stack::{Name, PinnedImage};
use crate::store::DeploymentId;

/// busybox 1.37, pinned by digest: it has `wget`, which names the address it connects to.
const IMAGE: &str = "docker.io/library/busybox:1.37@sha256:66a6306db78bf2dbf3487f293aa8d6990d8e506fdffab9cc43fe422becf886e4";
/// The stack of the temporary network, when no stack has one.
const PROBE_STACK: &str = "hoster-doctor";
/// The probe container's service name, so its name says what it is.
const PROBE_SERVICE: &str = "otlp-probe";
/// How long `wget` may take, in seconds, and the exec around it.
const WGET_TIMEOUT: u32 = 5;
const EXEC_TIMEOUT: Duration = Duration::from_secs(20);

/// Runs the probe; a fake stands in for it in the doctor's own tests.
#[async_trait]
pub trait ContainerPath: Send + Sync {
    /// What a container on a stack network saw, through the podman socket at `socket`; an error
    /// when the probe could not run at all.
    async fn probe(&self, socket: &Path) -> Result<Seen, String>;
}

/// [`ContainerPath`] through the real podman socket.
#[derive(Debug, Default, Clone, Copy)]
pub struct PodmanPath;

#[async_trait]
impl ContainerPath for PodmanPath {
    async fn probe(&self, socket: &Path) -> Result<Seen, String> {
        let runtime = PodmanRuntime::connect(&PodmanSocket::configured(socket))
            .await
            .map_err(|err| format!("cannot connect to podman: {err}"))?;
        ReceiverPath::new(&runtime).probe().await
    }
}

/// The probe over any [`ContainerRuntime`].
pub struct ReceiverPath<'a, R> {
    runtime: &'a R,
}

impl<'a, R: ContainerRuntime> ReceiverPath<'a, R> {
    pub fn new(runtime: &'a R) -> Self {
        Self { runtime }
    }

    /// Pulls the image, picks or makes a stack network, and runs `wget` in a container on it.
    /// The container, and a network made for the probe, are removed before it returns.
    pub async fn probe(&self) -> Result<Seen, String> {
        let url = format!("{}/v1/traces", OtlpReceiver::ENDPOINT);
        self.probe_at(&url, None).await
    }

    /// [`Self::probe`] posting to `url`, with `bearer` as the token if one is given: for the
    /// end-to-end test, whose proxy cannot listen on port 80.
    pub async fn probe_at(&self, url: &str, bearer: Option<&str>) -> Result<Seen, String> {
        let image: PinnedImage = IMAGE.parse().expect("the probe image is pinned");
        self.runtime
            .pull_image(&image)
            .await
            .map_err(|err| format!("cannot pull {IMAGE}: {err}"))?;
        let existing = self
            .runtime
            .list_managed_networks()
            .await
            .map_err(|err| format!("cannot list the stack networks: {err}"))?
            .into_iter()
            .find_map(|network| network.labels.get(label::STACK)?.parse::<Name>().ok());
        let (stack, temporary) = match existing {
            Some(stack) => (stack, None),
            None => {
                let stack: Name = PROBE_STACK.parse().expect("a valid stack name");
                let network = NetworkSpec::for_stack(&stack);
                self.runtime
                    .ensure_network(&network)
                    .await
                    .map_err(|err| format!("cannot create the network {}: {err}", network.name))?;
                (stack, Some(network))
            }
        };
        let seen = self.run_in(&stack, image, url, bearer).await;
        if let Some(network) = temporary
            && let Err(err) = self.runtime.remove_network(&network.name).await
        {
            tracing::warn!(network = %network.name, error = %err, "cannot remove the probe's network");
        }
        let mut seen = seen?;
        seen.version = self.runtime.version();
        seen.network = NetworkSpec::for_stack(&stack).name;
        Ok(seen)
    }

    /// Creates, starts and execs in the probe container on `stack`'s network, then removes it.
    async fn run_in(
        &self,
        stack: &Name,
        image: PinnedImage,
        url: &str,
        bearer: Option<&str>,
    ) -> Result<Seen, String> {
        let generation = ServiceGeneration {
            stack: stack.clone(),
            service: PROBE_SERVICE.parse().expect("a valid service name"),
            generation: std::process::id(),
        };
        // No deployment made it; its labels, which would name one, are dropped below.
        let deployment: DeploymentId = "0".parse().expect("a number");
        let mut spec = ContainerSpec::new(&generation, deployment, image);
        // Not hoster's: a running hoster neither adopts nor removes it.
        spec.labels.clear();
        spec.command = Some(vec!["sleep".to_owned(), "60".to_owned()]);
        spec.read_only = true;
        spec.stop_timeout = Duration::from_secs(1);
        let id = self
            .runtime
            .create_container(&spec)
            .await
            .map_err(|err| format!("cannot create the probe container: {err}"))?;
        let ran = async {
            self.runtime
                .start(&id)
                .await
                .map_err(|err| format!("cannot start the probe container: {err}"))?;
            let mut argv: Vec<String> = ["wget", "-O", "/dev/null", "-T"]
                .map(str::to_owned)
                .to_vec();
            argv.push(WGET_TIMEOUT.to_string());
            if let Some(token) = bearer {
                argv.push("--header".to_owned());
                argv.push(format!("Authorization: Bearer {token}"));
                argv.push("--header".to_owned());
                argv.push("Content-Type: application/x-protobuf".to_owned());
            }
            argv.push("--post-data=".to_owned());
            argv.push(url.to_owned());
            self.runtime
                .exec(&id, &argv, EXEC_TIMEOUT)
                .await
                .map(|outcome| Seen::from(outcome.output().as_ref()))
                .map_err(|err| format!("cannot run wget in the probe container: {err}"))
        }
        .await;
        if let Err(err) = self.runtime.remove(&id).await {
            tracing::warn!(container = %spec.name, error = %err, "cannot remove the probe container");
        }
        ran
    }
}

/// What the probe container's `wget` printed, and where it ran.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// The address `host.containers.internal` resolved to; `None` when it did not resolve.
    pub address: Option<IpAddr>,
    /// The HTTP status of the answer; `None` when there was none.
    pub status: Option<u16>,
    /// The last line `wget` printed, for a failure's detail.
    pub last_line: String,
    /// The podman version, for a failure's detail.
    pub version: String,
    /// The stack network the container ran on.
    pub network: String,
}

/// busybox `wget` writes `Connecting to <host> (<ip>:<port>)` before it connects, and for an
/// error status `wget: server returned error: HTTP/1.1 401 Unauthorized`; a success writes no
/// status, and is a 2xx.
impl From<&str> for Seen {
    fn from(output: &str) -> Self {
        let address = output.lines().find_map(|line| {
            let rest = line.strip_prefix("Connecting to ")?;
            let (_, inner) = rest.split_once('(')?;
            let (ip, _) = inner.trim_end_matches(')').rsplit_once(':')?;
            ip.parse().ok()
        });
        let status = output
            .lines()
            .find_map(|line| {
                let rest = line.split_once("HTTP/1.")?.1;
                rest.split_whitespace().nth(1)?.parse().ok()
            })
            .or_else(|| {
                // `-O /dev/null` writes no progress line; a clean exit had a 2xx.
                (address.is_some() && !output.contains("wget:")).then_some(200)
            });
        Self {
            address,
            status,
            last_line: output.lines().last().unwrap_or_default().trim().to_owned(),
            version: String::new(),
            network: String::new(),
        }
    }
}

/// How the check reads what the probe saw. `pasta` is `pasta --version`'s first line, for the
/// detail of a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Resolved to `169.254.1.2`, and the receiver answered.
    Reached { network: String },
    /// Something is wrong; why, and what to look at.
    Failed { detail: String, hint: String },
}

impl Seen {
    pub fn verdict(&self, pasta: &str) -> Verdict {
        let expected = IpAddr::V4(OtlpReceiver::HOST_ADDRESS);
        let versions = format!("podman {}, {pasta}", self.version);
        let host = OtlpReceiver::HOST;
        let network = &self.network;
        let failed = |detail: String, hint: &str| Verdict::Failed {
            detail: format!("{detail} ({versions})"),
            hint: hint.to_owned(),
        };
        match (self.address, self.status) {
            (None, _) => failed(
                format!(
                    "{host} does not resolve in a container on {network}: {}",
                    self.last_line
                ),
                "podman 5.3 or later with passt 2024_08_14 or later adds it to /etc/hosts; check \
                 host_containers_internal_ip in containers.conf (ADR-0069)",
            ),
            (Some(address), _) if address != expected => failed(
                format!("{host} resolves to {address} in a container on {network}, not {expected}"),
                "podman with passt 2024_08_14 or later maps 169.254.1.2 to the host; an older \
                 pasta, or host_containers_internal_ip in containers.conf, changes it (ADR-0069)",
            ),
            (Some(_), Some(401)) => Verdict::Reached {
                network: network.clone(),
            },
            (Some(_), Some(404)) => failed(
                format!(
                    "the proxy answered {host} from a container on {network} with 404: it did \
                     not see one of the host's own addresses as the peer"
                ),
                "the container's connection should arrive from the host's interface address \
                 (ADR-0069 §3); check pasta's options in containers.conf",
            ),
            (Some(_), Some(502)) => failed(
                format!("the proxy has no OTLP receiver behind {host}"),
                "restart hoster; the receiver listens on otlp.sock in the state directory",
            ),
            (Some(_), Some(status)) => failed(
                format!("{host} answered {status} from a container on {network}, not 401"),
                "is something other than hoster listening on port 80?",
            ),
            (Some(_), None) => failed(
                format!(
                    "nothing answered on {host}:80 from a container on {network}: {}",
                    self.last_line
                ),
                "is hoster running? Its proxy listens on port 80 on every address",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{ExecOutcome, FakeRuntime, Operation, RuntimeError};

    const REACHED: &str = "Connecting to host.containers.internal (169.254.1.2:80)\n\
        wget: server returned error: HTTP/1.1 401 Unauthorized\n";

    #[test]
    fn wget_output_is_read_for_the_address_and_the_status() {
        let seen = Seen::from(REACHED);
        assert_eq!(seen.address, Some("169.254.1.2".parse().unwrap()));
        assert_eq!(seen.status, Some(401));

        let refused = Seen::from(
            "Connecting to host.containers.internal (169.254.1.2:80)\n\
             wget: can't connect to remote host (169.254.1.2): Connection refused\n",
        );
        assert_eq!(refused.status, None);
        assert_eq!(
            refused.last_line,
            "wget: can't connect to remote host (169.254.1.2): Connection refused"
        );

        let unresolved = Seen::from("wget: bad address 'host.containers.internal'\n");
        assert_eq!((unresolved.address, unresolved.status), (None, None));

        let ok = Seen::from("Connecting to host.containers.internal (169.254.1.2:80)\n");
        assert_eq!(ok.status, Some(200));
    }

    #[test]
    fn only_the_receivers_401_from_169_254_1_2_passes() {
        let seen = |output: &str| Seen {
            version: "6.1.4".into(),
            network: "hoster-shop".into(),
            ..Seen::from(output)
        };
        assert_eq!(
            seen(REACHED).verdict("pasta 2026_10_02"),
            Verdict::Reached {
                network: "hoster-shop".into()
            }
        );
        for (output, needle) in [
            (
                "wget: bad address 'host.containers.internal'\n",
                "does not resolve",
            ),
            (
                "Connecting to host.containers.internal (10.0.2.2:80)\nwget: server returned error: HTTP/1.1 401 Unauthorized\n",
                "resolves to 10.0.2.2",
            ),
            (
                "Connecting to host.containers.internal (169.254.1.2:80)\nwget: server returned error: HTTP/1.1 404 Not Found\n",
                "404",
            ),
            (
                "Connecting to host.containers.internal (169.254.1.2:80)\nwget: can't connect to remote host (169.254.1.2): Connection refused\n",
                "nothing answered",
            ),
        ] {
            let Verdict::Failed { detail, .. } = seen(output).verdict("pasta 2026_10_02") else {
                panic!("{output} passed");
            };
            assert!(detail.contains(needle), "{detail}");
            // A failure names the podman and pasta versions.
            assert!(
                detail.ends_with("(podman 6.1.4, pasta 2026_10_02)"),
                "{detail}"
            );
        }
    }

    fn podman() -> FakeRuntime {
        let runtime = FakeRuntime::new();
        runtime.script_exec(
            PROBE_SERVICE,
            [Ok(ExecOutcome::finished(1, REACHED.into()))],
        );
        runtime
    }

    #[tokio::test]
    async fn the_probe_runs_on_a_stack_network_and_cleans_up() {
        let runtime = podman();
        runtime
            .ensure_network(&NetworkSpec::for_stack(&"shop".parse().unwrap()))
            .await
            .unwrap();
        let seen = ReceiverPath::new(&runtime).probe().await.unwrap();
        assert_eq!(seen.status, Some(401));
        assert_eq!(seen.network, "hoster-shop");
        assert_eq!(seen.version, runtime.version());
        // wget against the receiver's endpoint, in a container that is gone again.
        let execs = runtime.execs();
        let [(name, argv)] = execs.as_slice() else {
            panic!("{execs:?}");
        };
        assert!(name.starts_with("hoster-shop-otlp-probe-g"), "{name}");
        assert_eq!(argv[0], "wget");
        assert_eq!(
            argv.last().unwrap(),
            "http://host.containers.internal/v1/traces"
        );
        assert!(runtime.containers().is_empty());
        // The stack's network stays.
        assert_eq!(runtime.networks().len(), 1);
    }

    #[tokio::test]
    async fn without_a_stack_network_a_temporary_one_is_made_and_removed() {
        let runtime = podman();
        let seen = ReceiverPath::new(&runtime).probe().await.unwrap();
        assert_eq!(seen.network, "hoster-hoster-doctor");
        assert!(runtime.networks().is_empty());
        assert!(runtime.containers().is_empty());
    }

    #[tokio::test]
    async fn a_failed_start_still_removes_the_container() {
        let runtime = podman();
        runtime.fail_next(
            Operation::Start,
            RuntimeError::Api {
                operation: Operation::Start,
                message: "boom".into(),
            },
        );
        let err = ReceiverPath::new(&runtime).probe().await.unwrap_err();
        assert!(err.contains("cannot start"), "{err}");
        assert!(runtime.containers().is_empty());
        assert!(runtime.networks().is_empty());
    }

    /// The real probe through rootless podman (`TEST_HOSTER_PODMAN=1`): the name resolves to
    /// `169.254.1.2`; whether anything answers on port 80 depends on the machine.
    #[tokio::test]
    async fn the_probe_resolves_the_name_through_real_podman() {
        if !std::env::var("TEST_HOSTER_PODMAN").is_ok_and(|v| v == "1") {
            eprintln!("skipped: set TEST_HOSTER_PODMAN=1 to run against rootless podman");
            return;
        }
        let socket = std::env::var_os("HOSTER_PODMAN_SOCKET").map(std::path::PathBuf::from);
        let socket = PodmanSocket::from_env(socket.as_deref());
        let socket = socket.find().expect("a podman socket").to_owned();
        let seen = PodmanPath.probe(&socket).await.unwrap();
        assert_eq!(
            seen.address,
            Some("169.254.1.2".parse().unwrap()),
            "{seen:?}"
        );
        assert!(seen.network.starts_with("hoster-"), "{seen:?}");
        eprintln!("{seen:?}");
    }
}
