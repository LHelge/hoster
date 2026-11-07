//! [`FakeRuntime`]: an in-memory [`ContainerRuntime`] for deploy-engine and reconciler tests.
//!
//! It models just enough of podman to catch ordering mistakes: containers need their image
//! pulled and their network present, names are unique, networks with containers cannot be
//! removed. Failures are injected per [`Operation`], and every call is recorded.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};

use super::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerSpec, ContainerState, LogLine,
    LogOptions, NetworkSpec, ObjectKind, Operation, RuntimeError,
};
use crate::stack::PinnedImage;

/// An in-memory container runtime with failure injection.
#[derive(Debug, Default)]
pub struct FakeRuntime {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    networks: BTreeMap<String, NetworkSpec>,
    images: BTreeSet<String>,
    containers: BTreeMap<ContainerId, FakeContainer>,
    next_id: u64,
    /// One-shot failures, consumed in order.
    fail_next: HashMap<Operation, VecDeque<RuntimeError>>,
    /// Failures returned on every call until cleared.
    fail_always: HashMap<Operation, RuntimeError>,
    calls: Vec<Operation>,
}

#[derive(Debug, Clone)]
struct FakeContainer {
    spec: ContainerSpec,
    state: ContainerState,
    logs: Vec<LogLine>,
}

impl FakeContainer {
    fn info(&self, id: &ContainerId) -> ContainerInfo {
        ContainerInfo {
            id: id.clone(),
            name: self.spec.name.clone(),
            labels: self.spec.labels.clone(),
            state: self.state,
            ports: self.spec.ports.iter().copied().map(Into::into).collect(),
        }
    }
}

impl State {
    /// Records `operation` and returns an injected failure for it, if any.
    fn enter(&mut self, operation: Operation) -> Result<(), RuntimeError> {
        self.calls.push(operation);
        if let Some(err) = self
            .fail_next
            .get_mut(&operation)
            .and_then(VecDeque::pop_front)
        {
            return Err(err);
        }
        match self.fail_always.get(&operation) {
            Some(err) => Err(err.clone()),
            None => Ok(()),
        }
    }

    fn container(&mut self, id: &ContainerId) -> Result<&mut FakeContainer, RuntimeError> {
        self.containers
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound {
                kind: ObjectKind::Container,
                name: id.to_string(),
            })
    }
}

#[allow(
    dead_code,
    reason = "test helpers are used as the deploy engine's tests are written"
)]
impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panicking test thread must not hide the state from the assertions that follow.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Makes the next call of `operation` fail with `error` (queued after earlier ones).
    pub fn fail_next(&self, operation: Operation, error: RuntimeError) {
        self.lock()
            .fail_next
            .entry(operation)
            .or_default()
            .push_back(error);
    }

    /// Makes every call of `operation` fail with `error` until [`Self::clear_failures`].
    pub fn fail_always(&self, operation: Operation, error: RuntimeError) {
        self.lock().fail_always.insert(operation, error);
    }

    pub fn clear_failures(&self) {
        let mut state = self.lock();
        state.fail_next.clear();
        state.fail_always.clear();
    }

    /// Every call made so far, in order (failed ones included).
    pub fn calls(&self) -> Vec<Operation> {
        self.lock().calls.clone()
    }

    pub fn networks(&self) -> Vec<NetworkSpec> {
        self.lock().networks.values().cloned().collect()
    }

    pub fn has_image(&self, image: &PinnedImage) -> bool {
        self.lock().images.contains(&image.to_string())
    }

    /// Every container, managed or not.
    pub fn containers(&self) -> Vec<ContainerInfo> {
        let state = self.lock();
        state.containers.iter().map(|(id, c)| c.info(id)).collect()
    }

    /// The spec a container was created from.
    pub fn spec(&self, id: &ContainerId) -> Option<ContainerSpec> {
        self.lock().containers.get(id).map(|c| c.spec.clone())
    }

    /// Simulates something happening behind hoster's back, e.g. a crash (`Exited(Some(1))`).
    pub fn set_state(&self, id: &ContainerId, state: ContainerState) {
        if let Some(container) = self.lock().containers.get_mut(id) {
            container.state = state;
        }
    }

    /// Appends a line to a container's log.
    pub fn push_log(&self, id: &ContainerId, line: LogLine) {
        if let Some(container) = self.lock().containers.get_mut(id) {
            container.logs.push(line);
        }
    }
}

impl ContainerRuntime for FakeRuntime {
    async fn ensure_network(&self, network: &NetworkSpec) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::EnsureNetwork)?;
        state
            .networks
            .entry(network.name.clone())
            .or_insert_with(|| network.clone());
        Ok(())
    }

    async fn remove_network(&self, name: &str) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::RemoveNetwork)?;
        if !state.networks.contains_key(name) {
            return Err(RuntimeError::NotFound {
                kind: ObjectKind::Network,
                name: name.to_owned(),
            });
        }
        if state.containers.values().any(|c| c.spec.network == name) {
            return Err(RuntimeError::InUse {
                kind: ObjectKind::Network,
                name: name.to_owned(),
            });
        }
        state.networks.remove(name);
        Ok(())
    }

    async fn pull_image(&self, image: &PinnedImage) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::PullImage)?;
        state.images.insert(image.to_string());
        Ok(())
    }

    async fn create_container(&self, spec: &ContainerSpec) -> Result<ContainerId, RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::CreateContainer)?;
        let image = spec.image.to_string();
        if !state.images.contains(&image) {
            return Err(RuntimeError::NotFound {
                kind: ObjectKind::Image,
                name: image,
            });
        }
        if !state.networks.contains_key(&spec.network) {
            return Err(RuntimeError::NotFound {
                kind: ObjectKind::Network,
                name: spec.network.clone(),
            });
        }
        if state.containers.values().any(|c| c.spec.name == spec.name) {
            return Err(RuntimeError::Conflict {
                kind: ObjectKind::Container,
                name: spec.name.clone(),
            });
        }
        state.next_id += 1;
        let id = ContainerId::from(format!("{:064x}", state.next_id));
        state.containers.insert(
            id.clone(),
            FakeContainer {
                spec: spec.clone(),
                state: ContainerState::Created,
                logs: Vec::new(),
            },
        );
        Ok(id)
    }

    async fn start(&self, id: &ContainerId) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::Start)?;
        state.container(id)?.state = ContainerState::Running;
        Ok(())
    }

    async fn stop(&self, id: &ContainerId, _timeout: Duration) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::Stop)?;
        let container = state.container(id)?;
        if container.state.is_running() {
            container.state = ContainerState::Exited(Some(0));
        }
        Ok(())
    }

    async fn remove(&self, id: &ContainerId) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::Remove)?;
        state.container(id)?;
        state.containers.remove(id);
        Ok(())
    }

    async fn inspect(&self, id: &ContainerId) -> Result<ContainerInfo, RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::Inspect)?;
        Ok(state.container(id)?.info(id))
    }

    async fn list_managed(&self) -> Result<Vec<ContainerInfo>, RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::ListManaged)?;
        Ok(state
            .containers
            .iter()
            .map(|(id, c)| c.info(id))
            .filter(ContainerInfo::is_managed)
            .collect())
    }

    /// Yields the lines pushed so far; `follow` does not wait for more.
    fn logs(
        &self,
        id: &ContainerId,
        options: LogOptions,
    ) -> BoxStream<'static, Result<LogLine, RuntimeError>> {
        let mut state = self.lock();
        let lines = state
            .enter(Operation::Logs)
            .and_then(|()| Ok(state.container(id)?.logs.clone()));
        match lines {
            Ok(lines) => {
                let skip = options
                    .tail
                    .map_or(0, |tail| lines.len().saturating_sub(tail));
                stream::iter(lines.into_iter().skip(skip).map(Ok)).boxed()
            }
            Err(err) => stream::once(async move { Err(err) }).boxed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt;

    use super::super::{Labels, LogStream, PortBinding, PublishedPort, label};
    use super::*;

    fn image() -> PinnedImage {
        "ghcr.io/me/notes@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .parse()
            .unwrap()
    }

    fn network() -> NetworkSpec {
        NetworkSpec {
            name: "hoster-notes".into(),
            labels: Labels::from([(label::MANAGED.into(), "true".into())]),
        }
    }

    fn spec(name: &str) -> ContainerSpec {
        ContainerSpec {
            name: name.into(),
            image: image(),
            command: None,
            entrypoint: None,
            env: BTreeMap::new(),
            labels: Labels::from([
                (label::MANAGED.into(), "true".into()),
                (label::STACK.into(), "notes".into()),
            ]),
            network: "hoster-notes".into(),
            network_alias: "web".into(),
            ports: vec![PortBinding {
                container_port: 8080,
                host_port: 20000,
            }],
            volumes: Vec::new(),
            user: None,
            read_only: false,
            stop_timeout: Duration::from_secs(10),
        }
    }

    /// A runtime with the network and image in place.
    async fn ready() -> FakeRuntime {
        let runtime = FakeRuntime::new();
        runtime.ensure_network(&network()).await.unwrap();
        runtime.pull_image(&image()).await.unwrap();
        runtime
    }

    #[tokio::test]
    async fn container_lifecycle() {
        let runtime = ready().await;
        let id = runtime
            .create_container(&spec("hoster-notes-web-g1"))
            .await
            .unwrap();
        assert_eq!(
            runtime.inspect(&id).await.unwrap().state,
            ContainerState::Created
        );
        runtime.start(&id).await.unwrap();
        let info = runtime.inspect(&id).await.unwrap();
        assert_eq!(info.state, ContainerState::Running);
        assert_eq!(info.name, "hoster-notes-web-g1");
        assert_eq!(
            info.ports,
            spec("x")
                .ports
                .into_iter()
                .map(PublishedPort::from)
                .collect::<Vec<_>>()
        );
        assert!(info.ports.iter().all(PublishedPort::is_loopback));

        runtime.stop(&id, Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            runtime.inspect(&id).await.unwrap().state,
            ContainerState::Exited(Some(0))
        );
        runtime.remove(&id).await.unwrap();
        assert!(matches!(
            runtime.inspect(&id).await,
            Err(RuntimeError::NotFound {
                kind: ObjectKind::Container,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn ensure_network_is_idempotent() {
        let runtime = FakeRuntime::new();
        runtime.ensure_network(&network()).await.unwrap();
        runtime.ensure_network(&network()).await.unwrap();
        assert_eq!(runtime.networks(), vec![network()]);
    }

    #[tokio::test]
    async fn create_requires_image_and_network() {
        let runtime = FakeRuntime::new();
        let err = runtime.create_container(&spec("a")).await.unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::NotFound {
                kind: ObjectKind::Image,
                ..
            }
        ));
        runtime.pull_image(&image()).await.unwrap();
        assert!(runtime.has_image(&image()));
        let err = runtime.create_container(&spec("a")).await.unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::NotFound {
                kind: ObjectKind::Network,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn duplicate_names_conflict() {
        let runtime = ready().await;
        runtime.create_container(&spec("a")).await.unwrap();
        let err = runtime.create_container(&spec("a")).await.unwrap_err();
        assert_eq!(
            err,
            RuntimeError::Conflict {
                kind: ObjectKind::Container,
                name: "a".into()
            }
        );
    }

    #[tokio::test]
    async fn network_in_use_cannot_be_removed() {
        let runtime = ready().await;
        let id = runtime.create_container(&spec("a")).await.unwrap();
        assert!(matches!(
            runtime.remove_network("hoster-notes").await,
            Err(RuntimeError::InUse { .. })
        ));
        runtime.remove(&id).await.unwrap();
        runtime.remove_network("hoster-notes").await.unwrap();
        assert!(runtime.networks().is_empty());
        assert!(matches!(
            runtime.remove_network("hoster-notes").await,
            Err(RuntimeError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn list_managed_filters_by_label() {
        let runtime = ready().await;
        let managed = runtime.create_container(&spec("a")).await.unwrap();
        let mut foreign = spec("b");
        foreign.labels.clear();
        runtime.create_container(&foreign).await.unwrap();
        let listed = runtime.list_managed().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, managed);
        assert_eq!(runtime.containers().len(), 2);
    }

    #[tokio::test]
    async fn injected_failures() {
        let runtime = ready().await;
        let boom = RuntimeError::Api {
            operation: Operation::CreateContainer,
            message: "boom".into(),
        };
        runtime.fail_next(Operation::CreateContainer, boom.clone());
        assert_eq!(
            runtime.create_container(&spec("a")).await,
            Err(boom.clone())
        );
        // One-shot: the next call goes through, and the failed one left nothing behind.
        let id = runtime.create_container(&spec("a")).await.unwrap();

        runtime.fail_always(Operation::Start, boom.clone());
        assert_eq!(runtime.start(&id).await, Err(boom.clone()));
        assert_eq!(runtime.start(&id).await, Err(boom));
        runtime.clear_failures();
        runtime.start(&id).await.unwrap();

        assert_eq!(
            runtime.calls(),
            vec![
                Operation::EnsureNetwork,
                Operation::PullImage,
                Operation::CreateContainer,
                Operation::CreateContainer,
                Operation::Start,
                Operation::Start,
                Operation::Start,
            ]
        );
    }

    #[tokio::test]
    async fn crash_is_visible_to_inspect() {
        let runtime = ready().await;
        let id = runtime.create_container(&spec("a")).await.unwrap();
        runtime.start(&id).await.unwrap();
        runtime.set_state(&id, ContainerState::Exited(Some(137)));
        assert_eq!(
            runtime.inspect(&id).await.unwrap().state,
            ContainerState::Exited(Some(137))
        );
        assert_eq!(runtime.spec(&id), Some(spec("a")));
    }

    #[tokio::test]
    async fn logs_honour_tail_and_report_missing_containers() {
        let runtime = ready().await;
        let id = runtime.create_container(&spec("a")).await.unwrap();
        for text in ["one", "two", "three"] {
            runtime.push_log(
                &id,
                LogLine {
                    stream: LogStream::Stdout,
                    text: text.into(),
                },
            );
        }
        let all: Vec<_> = runtime
            .logs(&id, LogOptions::default())
            .try_collect()
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        let tail: Vec<_> = runtime
            .logs(
                &id,
                LogOptions {
                    follow: false,
                    tail: Some(2),
                },
            )
            .map_ok(|line| line.text)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(tail, ["two", "three"]);

        let missing = ContainerId::from("nope".to_owned());
        let result: Result<Vec<_>, _> = runtime
            .logs(&missing, LogOptions::default())
            .try_collect()
            .await;
        assert!(matches!(result, Err(RuntimeError::NotFound { .. })));
    }
}
