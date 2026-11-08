//! The services table of a stack's overview (epic 8bf, task yz5), which is also where the
//! stack's containers are listed now that `/containers` is gone (ADR-0054).
//!
//! One row per service of the active revision: its name with the name of the container that
//! runs it, the image reference with the short digest under it (in full as the tooltip) and a
//! "CI-built" tag, a state pill (healthy, running, unhealthy, crash looping, deploying, stopped,
//! waiting) with the health detail as its tooltip, the reconciler's restarts (ADR-0028), the
//! loopback port of a routed service (ADR-0026; `internal` for one that is not routed), its
//! generation, how long its container has been up (from `State.StartedAt`, which
//! only inspect reports) and a link to its log.
//!
//! The port is the one the running container publishes, as podman reports it: the allocation
//! itself (the port reservations in the store) is not part of [`StackStatus`], so a routed
//! service whose container is missing, or whose containers cannot be listed, shows `—`.
//!
//! A service that should run but has no container of its generation (removed outside hoster,
//! or not recreated by the reconciler yet) is shown stopped; when the containers cannot be
//! listed at all, its state comes from its health probes alone.
//!
//! Every other container of the stack follows its service as a muted row: the old generation
//! draining during a deploy, or the new one starting. A container whose service the active
//! revision no longer has is listed after the services. The containers are read from the
//! runtime; when that fails, the services are shown without them and the card says why.

use std::collections::BTreeSet;
use std::fmt;
use std::time::SystemTime;

use super::super::ShortDigest;
use super::super::list::{ServiceChip, StackCard};
use crate::health::HealthStatus;
use crate::orchestrator::{ServiceState, ServiceStatus, StackStatus};
use crate::runtime::{ContainerInfo, PublishedPort, label};
use crate::stack::Stack;
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

    /// The service and container whose log the logs tab shows, and the choices around them.
    /// `container`, when it is one of the stack's, wins and picks its service; otherwise
    /// `service` (the first service when it names none the tab offers) and the container that
    /// runs it, else that service's newest.
    pub fn pick_log(
        &self,
        status: &StackStatus,
        service: Option<&str>,
        container: Option<&str>,
    ) -> LogPick {
        let mut services: Vec<String> = status.services.iter().map(|s| s.name.clone()).collect();
        for placed in &self.0 {
            // A container without a service label gets no chip of its own (it is still
            // streamed when it is the one asked for).
            if !placed.service.is_empty() && !services.contains(&placed.service) {
                services.push(placed.service.clone());
            }
        }
        let named = container.and_then(|name| self.0.iter().find(|c| c.info.name == name));
        let chosen = match named {
            Some(placed) => Some(placed.service.clone()),
            None => service
                .filter(|name| services.iter().any(|s| s == name))
                .map(str::to_owned)
                .or_else(|| services.first().cloned()),
        };
        let Some(chosen) = chosen else {
            return LogPick::default();
        };
        let status_of = status.services.iter().find(|s| s.name == chosen);
        let current = status_of.and_then(|s| self.current(s));
        let running = match status_of.map(|s| &s.state) {
            Some(ServiceState::Running { generation, .. }) => Some(*generation),
            _ => None,
        };
        let streamed = named
            .or(current)
            .or_else(|| self.of(&chosen).next())
            .map(|c| c.info.name.clone());
        let containers = self
            .of(&chosen)
            .map(|c| LogCandidate {
                name: c.info.name.clone(),
                generation: c.generation,
                role: match (c.generation, running) {
                    _ if current.is_some_and(|current| current.info.id == c.info.id) => None,
                    _ if status_of.is_none() => Some(Role::Orphan),
                    (Some(theirs), Some(running)) if theirs > running => Some(Role::Next),
                    (Some(theirs), Some(running)) if theirs < running => Some(Role::Previous),
                    _ => Some(Role::Other),
                },
                state: c.info.state.to_string(),
            })
            .collect();
        LogPick {
            services,
            service: Some(chosen),
            containers,
            container: streamed,
            named: named.is_some(),
        }
    }
}

/// What the logs tab shows; see [`StackContainers::pick_log`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LogPick {
    /// The services to choose from: the active revision's, then those only containers have.
    pub services: Vec<String>,
    /// The chosen service; `None` when there is none to choose.
    pub service: Option<String>,
    /// The chosen service's containers, newest generation first.
    pub containers: Vec<LogCandidate>,
    /// The container whose log is shown.
    pub container: Option<String>,
    /// Whether the query named that container, rather than it running the service.
    pub named: bool,
}

/// A container the logs tab can show the log of.
#[derive(Debug, PartialEq, Eq)]
pub struct LogCandidate {
    pub name: String,
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
    /// `containers` the runtime reports, or why it could not, as of `now`.
    pub fn new(
        status: &StackStatus,
        stack: Option<&Stack>,
        containers: Result<StackContainers, String>,
        now: SystemTime,
    ) -> Self {
        let (containers, error) = match containers {
            Ok(containers) => (containers, None),
            Err(err) => (StackContainers::default(), Some(err)),
        };
        let routed: BTreeSet<String> = stack
            .into_iter()
            .flat_map(|stack| &stack.services)
            .filter(|(_, service)| !service.routes.is_empty())
            .map(|(name, _)| name.to_string())
            .collect();
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
                    routed.contains(&service.name),
                    &containers,
                    listed,
                    now,
                )
            })
            .collect();
        let orphans = containers
            .0
            .iter()
            .filter(|c| !status.services.iter().any(|s| s.name == c.service))
            .map(|c| ContainerRow::new(&status.name, c, Role::Orphan, now))
            .collect();
        Self {
            rows,
            orphans,
            containers: containers.0.len(),
            error,
        }
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
    pub restarts: u32,
    pub port: String,
    pub generation: Option<u32>,
    pub up: Option<Uptime>,
    pub logs: String,
    /// Whether it has a container whose log the logs tab can show.
    pub has_log: bool,
    /// The service's containers of other generations.
    pub others: Vec<ContainerRow>,
}

impl ServiceRow {
    fn new(
        stack: &str,
        service: &ServiceStatus,
        deploying: bool,
        routed: bool,
        containers: &StackContainers,
        listed: bool,
        now: SystemTime,
    ) -> Self {
        let current = containers.current(service);
        let (generation, digest) = match &service.state {
            ServiceState::Running { generation, digest } => (Some(*generation), Some(digest)),
            _ => (None, None),
        };
        let (state, detail) = Self::state(service, deploying, current.map(|c| &c.info), listed);
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
                ContainerRow::new(stack, c, role, now)
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
            restarts: service.restarts.restarts,
            port,
            generation,
            up: current.and_then(|c| Uptime::of(&c.info, now)),
            logs: format!("/stacks/{stack}/logs?service={}", service.name),
            has_log: current.is_some() || !others.is_empty(),
            others,
        }
    }

    /// The pill, with what is behind it as its tooltip, and that detail. Crash looping and
    /// failing probes come first, then a deploy that moves the service, then whether its
    /// container runs. `listed` says whether the containers could be listed, so whether a
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
        let health = service.health.last.clone();
        match &service.state {
            _ if service.is_crash_looping() => (
                Tone::Fail.pill("crash looping"),
                format!(
                    "restarted {} times; {}",
                    service.restarts.restarts,
                    health.unwrap_or_else(|| "keeps exiting".to_owned())
                ),
            ),
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
}

impl ContainerRow {
    fn new(stack: &str, container: &Placed, role: Role, now: SystemTime) -> Self {
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
            logs: format!(
                "/stacks/{stack}/logs?service={}&container={}",
                container.service, container.info.name
            ),
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
    use crate::web::stacks::tests::{NOTES, digest, notes_status};

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
        }
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
        let table = ServiceTable::new(&status, Some(&stack), Ok(containers), now);

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
        assert_eq!(
            api.others[0].logs,
            "/stacks/notes/logs?service=api&container=hoster-notes-api-g1"
        );
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

        // The logs tab picks the running container, a named one, or the first service's.
        let containers = StackContainers::from(vec![
            container("api", 1, ContainerState::Running),
            container("api", 2, ContainerState::Running),
            container("cache", 1, ContainerState::Exited(Some(0))),
        ]);
        let pick = containers.pick_log(&status, None, None);
        assert_eq!(pick.services, ["api", "storage", "cache"]);
        assert_eq!(pick.service.as_deref(), Some("api"));
        assert_eq!(pick.container.as_deref(), Some("hoster-notes-api-g2"));
        assert!(!pick.named);
        let roles: Vec<_> = pick
            .containers
            .iter()
            .map(|c| (c.name.as_str(), c.role))
            .collect();
        assert_eq!(
            roles,
            [
                ("hoster-notes-api-g2", None),
                ("hoster-notes-api-g1", Some(Role::Previous))
            ]
        );
        let pick = containers.pick_log(&status, Some("api"), Some("hoster-notes-api-g1"));
        assert_eq!(pick.container.as_deref(), Some("hoster-notes-api-g1"));
        assert!(pick.named);
        let pick = containers.pick_log(&status, Some("api"), Some("hoster-other-api-g1"));
        assert_eq!(
            pick.container.as_deref(),
            Some("hoster-notes-api-g2"),
            "a container of another stack is not looked up"
        );
        // A named container picks its service; an unknown service falls back to the first.
        let pick = containers.pick_log(&status, Some("api"), Some("hoster-notes-cache-g1"));
        assert_eq!(pick.service.as_deref(), Some("cache"));
        assert_eq!(pick.containers[0].role, Some(Role::Orphan));
        let pick = containers.pick_log(&status, Some("cache"), None);
        assert_eq!(pick.container.as_deref(), Some("hoster-notes-cache-g1"));
        let pick = containers.pick_log(&status, Some("nope"), None);
        assert_eq!(pick.service.as_deref(), Some("api"));
        let pick = containers.pick_log(&status, Some("storage"), None);
        assert_eq!(pick.service.as_deref(), Some("storage"));
        assert_eq!(pick.container, None);
        assert!(pick.containers.is_empty());

        // Without the containers, the services are still listed.
        status.services[0].state = ServiceState::Running {
            generation: 3,
            digest: digest(3),
        };
        let table = ServiceTable::new(&status, Some(&stack), Err("podman is down".into()), now);
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

        status.services[1].state = ServiceState::AwaitingFirstDigest;
        let (html, detail) = pill(&status.services[1], false, None);
        assert!(html.contains(">waiting</span>"), "{html}");
        assert_eq!(detail, "awaiting its first digest");
    }
}
