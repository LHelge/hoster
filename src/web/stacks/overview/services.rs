//! The services table of a stack's overview (epic 8bf, task yz5), which is also where the
//! stack's containers are listed now that `/containers` is gone (ADR-0054).
//!
//! One row per service of the active revision: its name with the name of the container that
//! runs it, the image reference with the short digest under it (in full as the tooltip) and a
//! "CI-built" tag, a state pill (healthy, running, unhealthy, crash looping, deploying, stopped,
//! waiting) with the health detail as its tooltip, the reconciler's restarts (ADR-0028), the
//! loopback port of a routed service (ADR-0026; `internal` for one that is not routed), its
//! generation, how long its container has been up (from `State.StartedAt`, which
//! only inspect reports) and a link to its log. Under the name come the services it names in
//! `depends_on` and its resource limits and runtime options (ADR-0059: `memory 512MiB · pids
//! 512 · cpus 1.5`, `shm_size 256MiB · tmpfs /tmp 64MiB · init`), and under the pill the health
//! check in effect (ADR-0056): the declared `exec`, `http` or `tcp` probe with its command,
//! path or port, or the default, all from the revision the reconciler and the monitor use (the
//! last committed deployment's, ADR-0056 Decision 3). A stopped service the reconciler holds
//! back for its dependencies is "waiting for db"; a crash-looping one names its last exit,
//! OOM kills included (ADR-0059 §6).
//!
//! The port is the one the running container publishes, as podman reports it: the allocation
//! itself (the port reservations in the store) is not part of [`StackStatus`], so a routed
//! service whose container is missing, or whose containers cannot be listed, shows `—`.
//!
//! A service that should run but has no container of its generation (removed outside hoster,
//! or not recreated by the reconciler yet) is shown stopped; when the containers cannot be
//! listed at all, its state comes from its health probes alone.
//!
//! Each container's row has its CPU and memory from the resource sampler (`usage.rs`, ADR-0066);
//! a service without a container shows `—` there.
//!
//! Every other container of the stack follows its service as a muted row: the old generation
//! draining during a deploy, or the new one starting. A container whose service the active
//! revision no longer has is listed after the services. The containers are read from the
//! runtime; when that fails, the services are shown without them and the card says why.

use std::collections::BTreeMap;
use std::fmt;
use std::time::SystemTime;

use super::super::ShortDigest;
use super::super::list::{ServiceChip, StackCard};
use super::usage::{StackUsage, UsageCells, UsageLimits};
use crate::health::{CheckSpec, HealthStatus};
use crate::orchestrator::{ServiceState, ServiceStatus, StackStatus};
use crate::runtime::{ContainerId, ContainerInfo, PublishedPort, label};
use crate::stack::{Limits, Service, Stack};
use crate::stats::ContainerUsage;
use crate::web::ui::Uptime;
use crate::web::ui::{Pill, Tone};

/// The managed containers of one stack, with the service and generation their labels name.
#[derive(Debug, Default)]
pub struct StackContainers(Vec<Placed>);

/// A container with where its labels put it.
#[derive(Debug)]
struct Placed {
    service: String,
    generation: Option<u32>,
    info: ContainerInfo,
}

impl From<Vec<ContainerInfo>> for StackContainers {
    /// Sorted by service, then newest generation first.
    fn from(containers: Vec<ContainerInfo>) -> Self {
        let mut placed: Vec<Placed> = containers
            .into_iter()
            .map(|info| Placed {
                service: info.labels.get(label::SERVICE).cloned().unwrap_or_default(),
                generation: info
                    .labels
                    .get(label::GENERATION)
                    .and_then(|g| g.parse().ok()),
                info,
            })
            .collect();
        placed.sort_by(|a, b| {
            (&a.service, b.generation, &a.info.name).cmp(&(&b.service, a.generation, &b.info.name))
        });
        Self(placed)
    }
}

impl Placed {
    /// Its CPU and memory cells, from `usage`.
    fn cells(&self, usage: &StackUsage<'_>) -> UsageCells {
        usage.cells(&self.info.id, &self.service, self.generation)
    }
}

impl StackContainers {
    /// The container that runs `service`: the one of the generation it runs.
    fn current(&self, service: &ServiceStatus) -> Option<&Placed> {
        let ServiceState::Running { generation, .. } = service.state else {
            return None;
        };
        self.of(&service.name)
            .find(|c| c.generation == Some(generation))
    }

    fn of<'a>(&'a self, service: &str) -> impl Iterator<Item = &'a Placed> + use<'a> {
        let service = service.to_owned();
        self.0.iter().filter(move |c| c.service == service)
    }

    /// The stack's containers, for the live log links of the logs tab: the active revision's
    /// services in order, then those only containers have, each service's containers as they
    /// are listed (newest generation first). A container without a service label comes last.
    pub fn live_logs(&self, status: &StackStatus) -> Vec<LogCandidate> {
        let mut services: Vec<&str> = status.services.iter().map(|s| s.name.as_str()).collect();
        for placed in &self.0 {
            if !services.contains(&placed.service.as_str()) {
                services.push(placed.service.as_str());
            }
        }
        let mut live = Vec::new();
        for service in services {
            let status_of = status.services.iter().find(|s| s.name == service);
            let current = status_of.and_then(|s| self.current(s));
            let running = match status_of.map(|s| &s.state) {
                Some(ServiceState::Running { generation, .. }) => Some(*generation),
                _ => None,
            };
            live.extend(self.of(service).map(|c| LogCandidate {
                name: c.info.name.clone(),
                service: c.service.clone(),
                generation: c.generation,
                role: match (c.generation, running) {
                    _ if current.is_some_and(|current| current.info.id == c.info.id) => None,
                    _ if status_of.is_none() => Some(Role::Orphan),
                    (Some(theirs), Some(running)) if theirs > running => Some(Role::Next),
                    (Some(theirs), Some(running)) if theirs < running => Some(Role::Previous),
                    _ => Some(Role::Other),
                },
                state: c.info.state.to_string(),
            }));
        }
        live
    }
}

/// A container whose live log the logs tab links (`/containers/{name}/logs`).
#[derive(Debug, PartialEq, Eq)]
pub struct LogCandidate {
    pub name: String,
    /// Its service label; empty without one.
    pub service: String,
    pub generation: Option<u32>,
    /// How it relates to its service; `None` for the one that runs it.
    pub role: Option<Role>,
    /// Its state, as podman names it.
    pub state: String,
}

/// The services card.
#[derive(Debug)]
pub struct ServiceTable {
    pub rows: Vec<ServiceRow>,
    /// Containers of services the active revision does not have.
    pub orphans: Vec<ContainerRow>,
    /// How many containers the stack has.
    pub containers: usize,
    /// Why the containers could not be listed.
    pub error: Option<String>,
}

impl ServiceTable {
    /// The services of `status` (whose active revision is `stack`, when it parses), with the
    /// `containers` the runtime reports, or why it could not, and what the resource sampler has
    /// about them (`usage`), as of `now`. Dependencies, checks and limits come from the revision
    /// the reconciler and the monitor use: the last committed deployment's (ADR-0056 Decision
    /// 3), else the active one.
    pub fn new(
        status: &StackStatus,
        stack: Option<&Stack>,
        containers: Result<StackContainers, String>,
        usage: BTreeMap<ContainerId, ContainerUsage>,
        now: SystemTime,
    ) -> Self {
        let (containers, error) = match containers {
            Ok(containers) => (containers, None),
            Err(err) => (StackContainers::default(), Some(err)),
        };
        let declarations = Declarations::new(status, stack);
        let limits = UsageLimits::new(status, &declarations);
        let usage = StackUsage::new(&limits, usage);
        let listed = error.is_none();
        let deploying = StackCard::deploying(status);
        let rows = status
            .services
            .iter()
            .map(|service| {
                let moving = deploying
                    .as_ref()
                    .is_some_and(|names| names.contains(service.name.as_str()));
                ServiceRow::new(
                    &status.name,
                    service,
                    moving,
                    Declared {
                        active: declarations.active(&service.name),
                        in_effect: declarations.in_effect(&service.name),
                    },
                    (&containers, &usage),
                    listed,
                    now,
                )
            })
            .collect();
        let orphans = containers
            .0
            .iter()
            .filter(|c| !status.services.iter().any(|s| s.name == c.service))
            .map(|c| ContainerRow::new(c, Role::Orphan, &usage, now))
            .collect();
        Self {
            rows,
            orphans,
            containers: containers.0.len(),
            error,
        }
    }
}

/// The revisions that declare a stack's services: the active one, and the last committed
/// deployment's, which the reconciler and the monitor use (ADR-0056 Decision 3).
pub(super) struct Declarations<'a> {
    active: Option<&'a Stack>,
    committed: Option<Stack>,
}

impl<'a> Declarations<'a> {
    /// The revisions of `status`, whose active one is `active` when it parses.
    pub(super) fn new(status: &StackStatus, active: Option<&'a Stack>) -> Self {
        Self {
            active,
            committed: status
                .committed
                .as_ref()
                .and_then(|revision| revision.toml.parse::<Stack>().ok()),
        }
    }

    /// `name` in the active revision.
    fn active(&self, name: &str) -> Option<&'a Service> {
        Self::service(self.active?, name)
    }

    /// `name` in the revision in effect: the committed one, else the active one.
    pub(super) fn in_effect(&self, name: &str) -> Option<&Service> {
        self.committed
            .as_ref()
            .and_then(|stack| Self::service(stack, name))
            .or_else(|| self.active(name))
    }

    fn service<'s>(stack: &'s Stack, name: &str) -> Option<&'s Service> {
        stack
            .services
            .iter()
            .find(|(n, _)| n.as_ref() == name)
            .map(|(_, service)| service)
    }
}

/// One service's row, with its other containers under it.
#[derive(Debug)]
pub struct ServiceRow {
    pub name: String,
    /// The container running it.
    pub container: Option<String>,
    pub image: String,
    pub ci_built: bool,
    pub digest: Option<String>,
    pub short_digest: Option<String>,
    pub state: Pill,
    /// The state for screen readers, which cannot reach the pill's tooltip.
    pub state_detail: String,
    /// The services it names in `depends_on`, in name order.
    pub depends_on: Vec<String>,
    /// How many config files the active revision gives it (ADR-0057); the files card lists
    /// them.
    pub files: usize,
    /// The health check in effect; `None` when no revision declares the service (it does not
    /// parse).
    pub check: Option<CheckView>,
    /// The resource limits in effect (ADR-0059 §2), `memory 512MiB · cpus 1.5`; `None` for
    /// none.
    pub limits: Option<String>,
    /// The runtime options in effect (ADR-0059 §1), `shm_size 256MiB · tmpfs /tmp 64MiB ·
    /// init`; `None` for none.
    pub options: Option<String>,
    pub restarts: u32,
    pub port: String,
    pub generation: Option<u32>,
    pub up: Option<Uptime>,
    pub logs: String,
    /// Whether it has a container whose log the logs tab can show.
    pub has_log: bool,
    /// The CPU and memory of the container running it; `None` when none does.
    pub usage: Option<UsageCells>,
    /// The service's containers of other generations.
    pub others: Vec<ContainerRow>,
}

impl ServiceRow {
    /// The row of `service`, as the revisions in `declared` declare it.
    fn new(
        stack: &str,
        service: &ServiceStatus,
        deploying: bool,
        declared: Declared<'_>,
        (containers, usage): (&StackContainers, &StackUsage<'_>),
        listed: bool,
        now: SystemTime,
    ) -> Self {
        let current = containers.current(service);
        let (generation, digest) = match &service.state {
            ServiceState::Running { generation, digest } => (Some(*generation), Some(digest)),
            _ => (None, None),
        };
        let (state, detail) = Self::state(service, deploying, current.map(|c| &c.info), listed);
        let routed = declared.active.is_some_and(|s| !s.routes.is_empty());
        let port = match current {
            _ if !routed => "internal".to_owned(),
            Some(current) => Ports(&current.info.ports).to_string(),
            None => "—".to_owned(),
        };
        let others: Vec<ContainerRow> = containers
            .of(&service.name)
            .filter(|c| current.is_none_or(|current| current.info.id != c.info.id))
            .map(|c| {
                let role = match (c.generation, generation) {
                    (Some(theirs), Some(running)) if theirs > running => Role::Next,
                    (Some(theirs), Some(running)) if theirs < running => Role::Previous,
                    _ => Role::Other,
                };
                ContainerRow::new(c, role, usage, now)
            })
            .collect();
        Self {
            name: service.name.clone(),
            container: current.map(|c| c.info.name.clone()),
            image: service.image.clone(),
            ci_built: service.ci_built,
            short_digest: digest.map(|d| ShortDigest(d).to_string()),
            digest: digest.cloned(),
            state,
            state_detail: detail,
            depends_on: declared
                .in_effect
                .into_iter()
                .flat_map(|s| &s.depends_on)
                .map(ToString::to_string)
                .collect(),
            files: declared.active.map_or(0, |s| s.files.len()),
            check: declared.in_effect.map(CheckView::from),
            limits: declared
                .in_effect
                .filter(|s| !s.limits.is_empty())
                .map(|s| LimitsView(&s.limits).to_string()),
            options: declared
                .in_effect
                .map(|s| OptionsView(s).to_string())
                .filter(|options| !options.is_empty()),
            restarts: service.restarts.restarts,
            port,
            generation,
            up: current.and_then(|c| Uptime::of(&c.info, now)),
            logs: format!("/stacks/{stack}/logs?service={}", service.name),
            has_log: current.is_some() || !others.is_empty(),
            usage: current.map(|c| c.cells(usage)),
            others,
        }
    }

    /// The pill, with what is behind it as its tooltip, and that detail. Crash looping comes
    /// first, then a stopped container the reconciler holds back for its dependencies (unless a
    /// deploy moves the service: the reconciler leaves a busy stack alone, so what it waits
    /// for is stale), then failing probes, then a deploy that moves the service, then whether
    /// its container runs. `listed` says whether the containers could be listed, so whether a
    /// missing `current` means the service has none.
    fn state(
        service: &ServiceStatus,
        deploying: bool,
        current: Option<&ContainerInfo>,
        listed: bool,
    ) -> (Pill, String) {
        let (pill, detail) = Self::judge(service, deploying, current, listed);
        (pill.title(&detail), detail)
    }

    fn judge(
        service: &ServiceStatus,
        deploying: bool,
        current: Option<&ContainerInfo>,
        listed: bool,
    ) -> (Pill, String) {
        let health = service.health.detail();
        match &service.state {
            _ if service.is_crash_looping() => {
                let mut detail = format!("restarted {} times", service.restarts.restarts);
                if let Some(last) = &service.restarts.last_exit {
                    detail.push_str(&format!("; last: {last}"));
                }
                match health {
                    Some(health) => detail.push_str(&format!("; {health}")),
                    None if service.restarts.last_exit.is_none() => {
                        detail.push_str("; keeps exiting");
                    }
                    None => {}
                }
                (Tone::Fail.pill("crash looping"), detail)
            }
            // Held back by the reconciler until its dependencies run and pass (ADR-0056), as
            // long as its container is known not to run.
            ServiceState::Running { .. }
                if !deploying
                    && !service.waiting_for.is_empty()
                    && match current {
                        Some(info) => !info.state.is_running(),
                        None => listed,
                    } =>
            {
                let deps = service.waiting_for.join(", ");
                (
                    Tone::Warn.pill(format!("waiting for {deps}")),
                    format!(
                        "stopped: the reconciler starts it once {deps} {}",
                        if service.waiting_for.len() == 1 {
                            "runs and passes its check"
                        } else {
                            "run and pass their checks"
                        }
                    ),
                )
            }
            _ if ServiceChip::failing(service) => (
                Tone::Fail.pill("unhealthy"),
                health.unwrap_or_else(|| "the health check fails".to_owned()),
            ),
            _ if deploying => (
                Tone::Progress.pill("deploying"),
                "a deployment is moving it to a new generation".to_owned(),
            ),
            ServiceState::Running { generation, .. } => match current {
                Some(info) if !info.state.is_running() => (
                    Tone::Neutral.pill("stopped"),
                    format!("container {}", info.state),
                ),
                None if listed => (
                    Tone::Neutral.pill("stopped"),
                    format!("no container of generation {generation}"),
                ),
                _ => match service.health.status {
                    HealthStatus::Healthy => (
                        Tone::Ok.pill("healthy"),
                        health.unwrap_or_else(|| "the health check passes".to_owned()),
                    ),
                    _ => (
                        Tone::Ok.pill("running"),
                        health.unwrap_or_else(|| "not probed yet".to_owned()),
                    ),
                },
            },
            ServiceState::AwaitingFirstDigest => (
                Tone::Neutral.pill("waiting"),
                "awaiting its first digest".to_owned(),
            ),
            ServiceState::NotDeployed => (Tone::Neutral.pill("stopped"), "not deployed".to_owned()),
        }
    }
}

/// How the revisions declare one service, when they parse and have it.
#[derive(Debug, Clone, Copy)]
struct Declared<'a> {
    /// In the active revision: its routes.
    active: Option<&'a Service>,
    /// In the revision the reconciler and the monitor use (the last committed deployment's,
    /// else the active one): its dependencies and check.
    in_effect: Option<&'a Service>,
}

/// The health check a service's containers get, as the overview shows it under the state.
#[derive(Debug, PartialEq, Eq)]
pub struct CheckView {
    /// `exec: pg_isready -U app`, `GET /healthz on container port 8080`.
    pub check: String,
    /// `declared` or `default`.
    pub source: &'static str,
    /// Why it applies, in full: `declared in health.exec`.
    pub why: &'static str,
}

impl From<&Service> for CheckView {
    fn from(service: &Service) -> Self {
        let spec = CheckSpec::from(service);
        Self {
            check: spec.to_string(),
            source: if spec.is_default() {
                "default"
            } else {
                "declared"
            },
            why: spec.why(),
        }
    }
}

/// A service's resource limits, in the order the table documents them: `memory 512MiB · pids
/// 512 · cpus 1.5`.
struct LimitsView<'a>(&'a Limits);

impl fmt::Display for LimitsView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Limits { memory, pids, cpus } = self.0;
        let parts = [
            memory.map(|m| format!("memory {m}")),
            pids.map(|p| format!("pids {p}")),
            cpus.map(|c| format!("cpus {c}")),
        ];
        f.write_str(&parts.into_iter().flatten().collect::<Vec<_>>().join(" · "))
    }
}

/// A service's runtime options: `shm_size 256MiB · tmpfs /tmp 64MiB, /run 16MiB · init`, empty
/// when it sets none.
struct OptionsView<'a>(&'a Service);

impl fmt::Display for OptionsView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let service = self.0;
        let tmpfs = (!service.tmpfs.is_empty()).then(|| {
            let mounts: Vec<String> = service
                .tmpfs
                .iter()
                .map(|(path, size)| format!("{path} {size}"))
                .collect();
            format!("tmpfs {}", mounts.join(", "))
        });
        let parts = [
            service.shm_size.map(|size| format!("shm_size {size}")),
            tmpfs,
            service.init.then(|| "init".to_owned()),
        ];
        f.write_str(&parts.into_iter().flatten().collect::<Vec<_>>().join(" · "))
    }
}

/// How a container that does not run its service relates to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// An older generation, draining during a deploy or not yet removed.
    Previous,
    /// A newer generation, starting during a deploy.
    Next,
    /// Some other generation of a service that runs none.
    Other,
    /// A container of a service the active revision does not have.
    Orphan,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::Previous => "previous generation",
            Role::Next => "next generation",
            Role::Other => "not the running generation",
            Role::Orphan => "service not in the active revision",
        })
    }
}

/// A container shown as a muted row.
#[derive(Debug)]
pub struct ContainerRow {
    pub name: String,
    pub service: String,
    pub role: Role,
    /// The container's state, as podman names it.
    pub state: String,
    pub port: String,
    pub generation: Option<u32>,
    pub up: Option<Uptime>,
    pub logs: String,
    /// Its own CPU and memory.
    pub usage: UsageCells,
}

impl ContainerRow {
    fn new(container: &Placed, role: Role, usage: &StackUsage<'_>, now: SystemTime) -> Self {
        Self {
            name: container.info.name.clone(),
            service: container.service.clone(),
            role,
            state: container.info.state.to_string(),
            port: if container.info.ports.is_empty() {
                "—".to_owned()
            } else {
                Ports(&container.info.ports).to_string()
            },
            generation: container.generation,
            up: Uptime::of(&container.info, now),
            logs: format!("/containers/{}/logs", container.info.name),
            usage: container.cells(usage),
        }
    }
}

/// A container's published ports: the host port, then the container port it reaches
/// (`20001 → 8080`); `—` for none. A binding off loopback (which hoster never makes, ADR-0007)
/// shows its address.
struct Ports<'a>(&'a [PublishedPort]);

impl fmt::Display for Ports<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("—");
        }
        for (i, port) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" · ")?;
            }
            if !port.is_loopback() {
                write!(f, "{}:", port.host_ip)?;
            }
            write!(f, "{} → {}", port.host_port, port.container_port)?;
        }
        Ok(())
    }
}

impl Uptime {
    /// The uptime of `container` at `now`, while it runs and its start time is known.
    fn of(container: &ContainerInfo, now: SystemTime) -> Option<Self> {
        let started = container
            .started_at
            .filter(|_| container.state.is_running())?;
        Some(Uptime(now.duration_since(started).unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use super::*;
    use crate::runtime::{ContainerId, ContainerState, Labels};
    use crate::web::stacks::tests::{NOTES, digest, notes_status, revision};

    fn container(service: &str, generation: u32, state: ContainerState) -> ContainerInfo {
        ContainerInfo {
            id: ContainerId::from(format!("{service}-{generation}")),
            name: format!("hoster-notes-{service}-g{generation}"),
            labels: Labels::from([
                (label::MANAGED.into(), "true".into()),
                (label::STACK.into(), "notes".into()),
                (label::SERVICE.into(), service.into()),
                (label::GENERATION.into(), generation.to_string()),
            ]),
            state,
            ports: Vec::new(),
            started_at: None,
            oom_killed: false,
        }
    }

    /// ADR-0056: the check and dependencies in effect come from the committed revision, and
    /// "waiting for" shows only while the container is known not to run and no deploy moves it.
    #[test]
    fn waits_show_while_stopped_and_checks_follow_the_committed_revision() {
        let now = SystemTime::now();
        let mut status = notes_status();
        let active = format!(
            "{}\n[services.storage.health]\nexec = [\"pg_isready\"]\n",
            NOTES.replace(
                "[services.storage]\n",
                "[services.storage]\ndepends_on = [\"api\"]\n"
            )
        );
        status.revision = revision("notes", 2, &active);
        status.committed = Some(revision("notes", 1, NOTES));
        status.services[1].state = ServiceState::Running {
            generation: 1,
            digest: digest(2),
        };
        status.services[1].waiting_for = vec!["api".into(), "cache".into()];
        let stack: Stack = active.parse().unwrap();
        let storage = |status: &StackStatus, containers: Vec<ContainerInfo>| {
            let table = ServiceTable::new(
                status,
                Some(&stack),
                Ok(StackContainers::from(containers)),
                BTreeMap::new(),
                now,
            );
            table.rows.into_iter().nth(1).unwrap()
        };

        // The committed revision is in effect: no dependency yet, the unrouted default.
        let row = storage(&status, Vec::new());
        assert!(row.depends_on.is_empty());
        assert_eq!(
            row.check,
            Some(CheckView {
                check: "running for 5s".into(),
                source: "default",
                why: "the default for a service without routes or health",
            })
        );
        // Without a container, and with a stopped one, it waits; plural for two.
        let waiting = "stopped: the reconciler starts it once api, cache run and pass their checks";
        assert_eq!(row.state_detail, waiting);
        let stopped = container("storage", 1, ContainerState::Exited(Some(0)));
        assert_eq!(
            storage(&status, vec![stopped.clone()]).state_detail,
            waiting
        );
        // A running container is not waiting, whatever the reconciler last recorded.
        let running = container("storage", 1, ContainerState::Running);
        assert_eq!(
            storage(&status, vec![running.clone()]).state_detail,
            "not probed yet"
        );
        // A deploy that moves it wins over a stale wait; crash looping wins over both.
        let service = &status.services[1];
        let (pill, detail) = ServiceRow::judge(service, true, Some(&stopped), true);
        assert_eq!(
            detail, "a deployment is moving it to a new generation",
            "{pill}"
        );
        let mut looping = service.clone();
        looping.restarts.crash_looping = true;
        let (_, detail) = ServiceRow::judge(&looping, false, Some(&stopped), true);
        assert!(detail.starts_with("restarted "), "{detail}");

        // Nothing committed: the active revision is in effect.
        status.committed = None;
        status.services[1].waiting_for = vec!["api".into()];
        let row = storage(&status, Vec::new());
        assert_eq!(row.depends_on, ["api"]);
        assert_eq!(
            row.check.as_ref().map(|c| (c.check.as_str(), c.source)),
            Some(("exec: pg_isready", "declared"))
        );
        assert_eq!(
            row.state_detail,
            "stopped: the reconciler starts it once api runs and passes its check"
        );
        // A revision that does not parse declares nothing.
        status.revision = revision("notes", 3, "not toml");
        let table = ServiceTable::new(
            &status,
            None,
            Ok(StackContainers::default()),
            BTreeMap::new(),
            now,
        );
        assert!(
            table
                .rows
                .iter()
                .all(|r| r.check.is_none() && r.depends_on.is_empty())
        );
    }

    #[test]
    fn ports_name_the_host_and_container_port() {
        let loopback = PublishedPort {
            container_port: 8080,
            host_ip: Ipv4Addr::LOCALHOST.into(),
            host_port: 20001,
        };
        let open = PublishedPort {
            host_ip: Ipv4Addr::UNSPECIFIED.into(),
            ..loopback
        };
        assert_eq!(Ports(&[loopback]).to_string(), "20001 → 8080");
        assert_eq!(
            Ports(&[loopback, open]).to_string(),
            "20001 → 8080 · 0.0.0.0:20001 → 8080"
        );
        assert_eq!(Ports(&[]).to_string(), "—");
    }

    #[test]
    fn rows_show_the_running_container_and_the_others_under_it() {
        let now = SystemTime::now();
        let mut status = notes_status();
        // api runs generation 2 (notes_status); storage is not deployed.
        let mut running = container("api", 2, ContainerState::Running);
        running.ports = vec![PublishedPort {
            container_port: 8080,
            host_ip: Ipv4Addr::LOCALHOST.into(),
            host_port: 20004,
        }];
        running.started_at = Some(now - Duration::from_secs(2 * 3_600 + 4 * 60));
        let draining = container("api", 1, ContainerState::Running);
        let orphan = container("cache", 1, ContainerState::Exited(Some(0)));
        let containers = StackContainers::from(vec![orphan, draining, running]);
        let stack: Stack = NOTES.parse().unwrap();
        let table = ServiceTable::new(&status, Some(&stack), Ok(containers), BTreeMap::new(), now);

        let api = &table.rows[0];
        assert_eq!(api.container.as_deref(), Some("hoster-notes-api-g2"));
        assert_eq!(api.port, "20004 → 8080");
        assert_eq!(api.generation, Some(2));
        assert_eq!(api.up.map(|u| u.to_string()).as_deref(), Some("2h 04m"));
        assert_eq!(api.logs, "/stacks/notes/logs?service=api");
        assert_eq!(api.short_digest, Some(format!("sha256:{}", "01".repeat(6))));
        assert_eq!(api.others.len(), 1);
        assert_eq!(api.others[0].name, "hoster-notes-api-g1");
        assert_eq!(api.others[0].role, Role::Previous);
        assert_eq!(api.others[0].logs, "/containers/hoster-notes-api-g1/logs");
        assert_eq!(api.others[0].up, None, "no start time is known");

        let storage = &table.rows[1];
        assert_eq!(storage.port, "internal", "storage is not routed");
        assert_eq!(storage.container, None);
        assert_eq!(storage.state_detail, "not deployed");
        assert!(!storage.has_log, "storage has no container");
        assert!(api.has_log);
        assert_eq!(table.orphans.len(), 1);
        assert_eq!(table.orphans[0].role, Role::Orphan);
        assert_eq!(table.orphans[0].state, "exited (0)");
        assert_eq!(table.containers, 3);

        // The logs tab links every container's live log, by service, with its role.
        let containers = StackContainers::from(vec![
            container("api", 1, ContainerState::Running),
            container("api", 2, ContainerState::Running),
            container("cache", 1, ContainerState::Exited(Some(0))),
        ]);
        let live: Vec<_> = containers
            .live_logs(&status)
            .into_iter()
            .map(|c| (c.name, c.service, c.generation, c.role))
            .collect();
        assert_eq!(
            live,
            [
                ("hoster-notes-api-g2".into(), "api".into(), Some(2), None),
                (
                    "hoster-notes-api-g1".into(),
                    "api".into(),
                    Some(1),
                    Some(Role::Previous)
                ),
                (
                    "hoster-notes-cache-g1".into(),
                    "cache".into(),
                    Some(1),
                    Some(Role::Orphan)
                ),
            ]
        );

        // Without the containers, the services are still listed.
        status.services[0].state = ServiceState::Running {
            generation: 3,
            digest: digest(3),
        };
        let table = ServiceTable::new(
            &status,
            Some(&stack),
            Err("podman is down".into()),
            BTreeMap::new(),
            now,
        );
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0].port, "—");
        assert_eq!(table.error.as_deref(), Some("podman is down"));
    }

    #[test]
    fn the_state_pill_puts_crashes_and_failing_probes_first() {
        let mut status = notes_status();
        let api = &mut status.services[0];
        // Without a container listing, a missing container says nothing about the service.
        let pill = |service: &ServiceStatus, deploying, current: Option<&ContainerInfo>| {
            let (pill, detail) = ServiceRow::state(service, deploying, current, false);
            (pill.to_string(), detail)
        };
        let (html, detail) = pill(api, false, None);
        assert!(html.contains(">running</span>"), "{html}");
        assert_eq!(detail, "not probed yet");
        // A watched generation not probed yet: its check is no result.
        api.health.check = Some("exec: pg_isready -U postgres, declared in health.exec".into());
        let (_, detail) = pill(api, false, None);
        assert_eq!(detail, "not probed yet");
        api.health.check = None;

        // With one, a service that has no container of its generation is not running.
        let (html, detail) = ServiceRow::state(api, false, None, true);
        assert!(html.to_string().contains(">stopped</span>"), "{html}");
        assert_eq!(detail, "no container of generation 2");
        let running = container("api", 2, ContainerState::Running);
        let (html, _) = ServiceRow::state(api, false, Some(&running), true);
        assert!(html.to_string().contains(">running</span>"), "{html}");

        let exited = container("api", 2, ContainerState::Exited(Some(1)));
        let (html, detail) = pill(api, false, Some(&exited));
        assert!(html.contains(">stopped</span>"), "{html}");
        assert_eq!(detail, "container exited (1)");

        let (html, _) = pill(api, true, None);
        assert!(html.contains(">deploying</span>"), "{html}");

        api.health.status = HealthStatus::Healthy;
        api.health.last = Some("GET /healthz: 200".into());
        let (html, detail) = pill(api, false, None);
        assert!(html.contains("pill-ok"), "{html}");
        assert!(html.contains(">healthy</span>"), "{html}");
        assert_eq!(detail, "GET /healthz: 200");
        // The monitor names the check in effect and why (ADR-0056).
        api.health.check = Some("exec: pg_isready -U postgres, declared in health.exec".into());
        let (_, detail) = pill(api, false, None);
        assert_eq!(
            detail,
            "GET /healthz: 200 · exec: pg_isready -U postgres, declared in health.exec"
        );
        api.health.check = None;

        api.health.status = HealthStatus::Unhealthy;
        api.health.last = Some("GET /healthz: <503>".into());
        let (html, detail) = pill(api, true, None);
        assert!(html.contains(">unhealthy</span>"), "{html}");
        assert!(
            html.contains(r#"title="GET /healthz: &#60;503&#62;""#),
            "{html}"
        );
        assert_eq!(detail, "GET /healthz: <503>");

        api.restarts.restarts = 6;
        api.restarts.crash_looping = true;
        let (html, detail) = pill(api, false, None);
        assert!(html.contains(">crash looping</span>"), "{html}");
        assert_eq!(detail, "restarted 6 times; GET /healthz: <503>");
        // The last exit comes first, an OOM kill or a failed start named (ADR-0059 §6).
        api.restarts.last_exit = Some(crate::orchestrator::ExitReason::StartFailed(
            "podman failed to start container: boom".into(),
        ));
        let (_, detail) = pill(api, false, None);
        assert_eq!(
            detail,
            "restarted 6 times; last: cannot start: podman failed to start container: boom; \
             GET /healthz: <503>"
        );

        status.services[1].state = ServiceState::AwaitingFirstDigest;
        let (html, detail) = pill(&status.services[1], false, None);
        assert!(html.contains(">waiting</span>"), "{html}");
        assert_eq!(detail, "awaiting its first digest");
    }
}
