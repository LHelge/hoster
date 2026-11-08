//! [`FakeRuntime`]: an in-memory [`ContainerRuntime`] for deploy-engine and reconciler tests.
//!
//! It models just enough of podman to catch ordering mistakes: containers need their image
//! pulled and their network and volumes present, names are unique, and networks and volumes
//! with containers cannot be removed. Failures are injected per [`Operation`], and every call
//! is recorded.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use tokio::sync::{Semaphore, mpsc};

use super::{
    ContainerId, ContainerInfo, ContainerRuntime, ContainerSpec, ContainerState, LogLine,
    LogOptions, NetworkSpec, ObjectKind, Operation, RuntimeError, VolumeSpec,
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
    volumes: BTreeMap<String, VolumeSpec>,
    images: BTreeSet<String>,
    containers: BTreeMap<ContainerId, FakeContainer>,
    next_id: u64,
    /// One-shot failures, consumed in order.
    fail_next: HashMap<Operation, VecDeque<RuntimeError>>,
    /// Failures returned on every call until cleared.
    fail_always: HashMap<Operation, RuntimeError>,
    calls: Vec<Operation>,
    /// Pulls of images whose reference contains the pattern wait at the hold.
    holds: Vec<(String, Arc<PullHold>)>,
    /// Removals of images whose reference contains the pattern wait at the hold.
    removal_holds: Vec<(String, Arc<PullHold>)>,
}

/// A point where pulls (or image removals) wait until the test releases them
/// ([`FakeRuntime::hold_pulls`], [`FakeRuntime::hold_image_removals`]), for tests that
/// interleave concurrent work deterministically.
#[derive(Debug)]
pub struct PullHold {
    /// One permit per pull that reached the hold.
    arrived: Semaphore,
    /// Closed on release.
    open: Semaphore,
}

impl PullHold {
    /// Waits until a pull is waiting at the hold.
    pub async fn reached(&self) {
        self.arrived
            .acquire()
            .await
            .expect("the arrival semaphore is never closed")
            .forget();
    }

    /// Lets every waiting and later pull through.
    pub fn release(&self) {
        self.open.close();
    }

    async fn pass(&self) {
        self.arrived.add_permits(1);
        // Closed means released; a permit is never added.
        let _ = self.open.acquire().await;
    }
}

#[derive(Debug, Clone)]
struct FakeContainer {
    spec: ContainerSpec,
    state: ContainerState,
    /// When it last started running.
    started_at: Option<SystemTime>,
    logs: Vec<LogLine>,
    /// The streams following the log; each gets every line pushed from now on. They end when
    /// the container stops running, as podman's followed logs do.
    followers: Vec<mpsc::UnboundedSender<LogLine>>,
}

impl FakeContainer {
    /// Sets the state; a container that stops running ends the streams following its log.
    fn set_state(&mut self, state: ContainerState) {
        if state.is_running() && !self.state.is_running() {
            self.started_at = Some(SystemTime::now());
        }
        self.state = state;
        if !state.is_running() {
            self.followers.clear();
        }
    }

    fn info(&self, id: &ContainerId) -> ContainerInfo {
        ContainerInfo {
            id: id.clone(),
            name: self.spec.name.clone(),
            labels: self.spec.labels.clone(),
            state: self.state,
            ports: self.spec.ports.iter().copied().map(Into::into).collect(),
            started_at: self.started_at,
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

    /// Makes every pull of an image whose reference contains `pattern` wait until the returned
    /// hold is released.
    pub fn hold_pulls(&self, pattern: &str) -> Arc<PullHold> {
        let hold = Arc::new(PullHold {
            arrived: Semaphore::new(0),
            open: Semaphore::new(0),
        });
        self.lock()
            .holds
            .push((pattern.to_owned(), Arc::clone(&hold)));
        hold
    }

    /// Makes every removal of an image whose reference contains `pattern` wait, before it
    /// changes anything, until the returned hold is released.
    pub fn hold_image_removals(&self, pattern: &str) -> Arc<PullHold> {
        let hold = Arc::new(PullHold {
            arrived: Semaphore::new(0),
            open: Semaphore::new(0),
        });
        self.lock()
            .removal_holds
            .push((pattern.to_owned(), Arc::clone(&hold)));
        hold
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

    /// Every volume that currently exists.
    pub fn volumes(&self) -> Vec<VolumeSpec> {
        self.lock().volumes.values().cloned().collect()
    }

    /// The names of every volume that currently exists.
    pub fn volume_names(&self) -> Vec<String> {
        self.lock().volumes.keys().cloned().collect()
    }

    pub fn has_image(&self, image: &PinnedImage) -> bool {
        self.lock().images.contains(&image.to_string())
    }

    /// Every image present, by its `registry/repository@digest` reference.
    pub fn images(&self) -> Vec<String> {
        self.lock().images.iter().cloned().collect()
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
            container.set_state(state);
        }
    }

    /// Sets when a container last started, as if it had been up since `time`.
    pub fn set_started_at(&self, id: &ContainerId, time: SystemTime) {
        if let Some(container) = self.lock().containers.get_mut(id) {
            container.started_at = Some(time);
        }
    }

    /// Appends a line to a container's log, and sends it to the streams following it.
    pub fn push_log(&self, id: &ContainerId, line: LogLine) {
        if let Some(container) = self.lock().containers.get_mut(id) {
            container
                .followers
                .retain(|follower| follower.send(line.clone()).is_ok());
            container.logs.push(line);
        }
    }

    /// How many streams still follow a container's log; a dropped stream no longer counts.
    pub fn log_followers(&self, id: &ContainerId) -> usize {
        let mut state = self.lock();
        let Some(container) = state.containers.get_mut(id) else {
            return 0;
        };
        container.followers.retain(|follower| !follower.is_closed());
        container.followers.len()
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

    async fn ensure_volume(&self, volume: &VolumeSpec) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::EnsureVolume)?;
        match state.volumes.get(&volume.name) {
            Some(existing) => volume.check_existing(&existing.labels),
            None => {
                state.volumes.insert(volume.name.clone(), volume.clone());
                Ok(())
            }
        }
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

    async fn list_managed_networks(&self) -> Result<Vec<NetworkSpec>, RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::ListNetworks)?;
        Ok(state
            .networks
            .values()
            .filter(|n| n.is_managed())
            .cloned()
            .collect())
    }

    async fn pull_image(&self, image: &PinnedImage) -> Result<(), RuntimeError> {
        let reference = image.to_string();
        let hold = self
            .lock()
            .holds
            .iter()
            .find(|(pattern, _)| reference.contains(pattern.as_str()))
            .map(|(_, hold)| Arc::clone(hold));
        if let Some(hold) = hold {
            hold.pass().await;
        }
        let mut state = self.lock();
        state.enter(Operation::PullImage)?;
        state.images.insert(image.to_string());
        Ok(())
    }

    async fn remove_image(&self, image: &PinnedImage) -> Result<(), RuntimeError> {
        let reference = image.to_string();
        let hold = self
            .lock()
            .removal_holds
            .iter()
            .find(|(pattern, _)| reference.contains(pattern.as_str()))
            .map(|(_, hold)| Arc::clone(hold));
        if let Some(hold) = hold {
            hold.pass().await;
        }
        let mut state = self.lock();
        state.enter(Operation::RemoveImage)?;
        if !state.images.contains(&reference) {
            return Err(RuntimeError::NotFound {
                kind: ObjectKind::Image,
                name: reference,
            });
        }
        if state
            .containers
            .values()
            .any(|c| c.spec.image.to_string() == reference)
        {
            return Err(RuntimeError::InUse {
                kind: ObjectKind::Image,
                name: reference,
            });
        }
        state.images.remove(&reference);
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
        if let Some(missing) = spec
            .volumes
            .iter()
            .find(|v| !state.volumes.contains_key(v.volume()))
        {
            return Err(RuntimeError::NotFound {
                kind: ObjectKind::Volume,
                name: missing.volume().to_owned(),
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
                started_at: None,
                logs: Vec::new(),
                followers: Vec::new(),
            },
        );
        Ok(id)
    }

    async fn start(&self, id: &ContainerId) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::Start)?;
        state.container(id)?.set_state(ContainerState::Running);
        Ok(())
    }

    async fn stop(&self, id: &ContainerId, _timeout: Duration) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::Stop)?;
        let container = state.container(id)?;
        if container.state.is_running() {
            container.set_state(ContainerState::Exited(Some(0)));
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

    async fn remove_volume(&self, name: &str) -> Result<(), RuntimeError> {
        let mut state = self.lock();
        state.enter(Operation::RemoveVolume)?;
        if !state.volumes.contains_key(name) {
            return Err(RuntimeError::NotFound {
                kind: ObjectKind::Volume,
                name: name.to_owned(),
            });
        }
        if state
            .containers
            .values()
            .any(|c| c.spec.volumes.iter().any(|v| v.volume() == name))
        {
            return Err(RuntimeError::InUse {
                kind: ObjectKind::Volume,
                name: name.to_owned(),
            });
        }
        state.volumes.remove(name);
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

    /// Yields the lines pushed so far (`since` and `tail` applied, `since` to the whole second
    /// as podman does). With `follow`, a running container's stream then yields every line
    /// pushed until the container stops running or the stream is dropped.
    fn logs(
        &self,
        id: &ContainerId,
        options: LogOptions,
    ) -> BoxStream<'static, Result<LogLine, RuntimeError>> {
        let mut state = self.lock();
        let found = state.enter(Operation::Logs).and_then(|()| {
            let container = state.container(id)?;
            let follower = (options.follow && container.state.is_running()).then(|| {
                let (tx, rx) = mpsc::unbounded_channel();
                container.followers.push(tx);
                rx
            });
            Ok((container.logs.clone(), follower))
        });
        let (mut lines, follower) = match found {
            Ok(found) => found,
            Err(err) => return stream::once(async move { Err(err) }).boxed(),
        };
        if let Some(since) = options.since {
            let second = since
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let since = std::time::UNIX_EPOCH + Duration::from_secs(second);
            lines.retain(|line| line.time.is_none_or(|time| time >= since));
        }
        let skip = options
            .tail
            .map_or(0, |tail| lines.len().saturating_sub(tail));
        let existing = stream::iter(lines.into_iter().skip(skip).map(Ok));
        match follower {
            Some(rx) => existing
                .chain(stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|line| (Ok(line), rx))
                }))
                .boxed(),
            None => existing.boxed(),
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
            secrets: BTreeMap::new(),
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
        assert_eq!(runtime.inspect(&id).await.unwrap().started_at, None);
        runtime.start(&id).await.unwrap();
        let started = runtime.inspect(&id).await.unwrap().started_at;
        assert!(started.is_some(), "starting sets the start time");
        runtime.set_state(&id, ContainerState::Exited(Some(137)));
        let crashed = runtime.inspect(&id).await.unwrap();
        assert_eq!(crashed.state, ContainerState::Exited(Some(137)));
        assert_eq!(crashed.started_at, started, "the last start is kept");
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
                    time: None,
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
                    tail: Some(2),
                    ..LogOptions::default()
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

    #[tokio::test]
    async fn followed_logs_stream_new_lines_until_stopped_or_dropped() {
        let runtime = ready().await;
        let id = runtime.create_container(&spec("a")).await.unwrap();
        runtime.start(&id).await.unwrap();
        let line = |text: &str| LogLine {
            stream: LogStream::Stderr,
            time: None,
            text: text.into(),
        };
        runtime.push_log(&id, line("before"));
        let follow = LogOptions {
            follow: true,
            ..LogOptions::default()
        };
        let mut stream = runtime.logs(&id, follow);
        assert_eq!(stream.next().await, Some(Ok(line("before"))));
        runtime.push_log(&id, line("after"));
        assert_eq!(stream.next().await, Some(Ok(line("after"))));
        assert_eq!(runtime.log_followers(&id), 1);
        drop(stream);
        assert_eq!(runtime.log_followers(&id), 0);

        let mut stream = runtime.logs(&id, follow);
        assert_eq!(stream.next().await.unwrap().unwrap().text, "before");
        runtime.stop(&id, Duration::ZERO).await.unwrap();
        assert_eq!(stream.next().await.unwrap().unwrap().text, "after");
        assert_eq!(stream.next().await, None, "stopping ends the stream");
    }

    #[tokio::test]
    async fn logs_since_keep_the_whole_starting_second() {
        let runtime = ready().await;
        let id = runtime.create_container(&spec("a")).await.unwrap();
        let at = |secs: u64, millis: u64| {
            std::time::UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis)
        };
        for (text, time) in [
            ("old", at(99, 900)),
            ("same", at(100, 100)),
            ("new", at(101, 0)),
        ] {
            runtime.push_log(
                &id,
                LogLine {
                    stream: LogStream::Stdout,
                    time: Some(time),
                    text: text.into(),
                },
            );
        }
        let texts: Vec<_> = runtime
            .logs(
                &id,
                LogOptions {
                    since: Some(at(100, 500)),
                    ..LogOptions::default()
                },
            )
            .map_ok(|line| line.text)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(texts, ["same", "new"]);
    }
}
