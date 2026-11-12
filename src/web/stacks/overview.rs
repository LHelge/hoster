//! `GET /stacks/{name}`: a stack's overview tab (ADR-0054, epic 8bf), under the shared
//! [`StackHeader`].
//!
//! The overview opens with the stack's KPI tiles and its responses by status over the last
//! hour of traffic history (ADR-0062 §8, the [`TrafficWindow`] the stack cards show too), then
//! shows the services with their containers, the routes
//! and named volumes of the active revision, the stack's Recent events (ADR-0055, kept live by
//! their own stream at `/stacks/{name}/events`, [`StackFeed`]), the form that deploys explicit
//! digests to the CI-built services, a link to apply a new revision, the refused webhook deploys
//! (ADR-0047) and, last, the danger zone: removing the stack once its name is typed (ADR-0030).
//! The danger zone says what goes (containers, network, routes) and what stays (revisions,
//! history, and the volumes unless they are ticked for deletion), and lists the volumes ticking
//! deletes: those of every revision, as [`Orchestrator::volumes`] finds them, with where the
//! active revision mounts each.
//!
//! The tiles, the responses by status and the routes card are built in `figures.rs`; the services
//! table, which lists the stack's containers too, in `services.rs`; its CPU and Memory cells,
//! which the same stream keeps live after each resource sampler tick (ADR-0066 §9), in
//! `usage.rs`.
//!
//! [`Orchestrator::volumes`]: crate::orchestrator::Orchestrator::volumes

mod figures;
mod services;
mod usage;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use askama::Template;
use axum::Extension;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};

use self::figures::{Figures, Readings, RouteRow, StatusChart};
#[cfg(test)]
pub(super) use self::services::Role;
pub(super) use self::services::{LogCandidate, LogPick, StackContainers};
use self::services::{ServiceRow, ServiceTable};
pub(in crate::web) use self::usage::UsageFeed;
use super::frame::{StackHeader, Tab};
use super::list::{StackCard, TrafficWindow};
use super::{Outcome, Stacks, UpdatesCard, When};
use crate::orchestrator::{StackSecret, StackStatus};
use crate::runtime::VolumeMount;
use crate::stack::{ConfigFile, ContainerPath, FileSource, Ownership, Stack};
use crate::stats::{Range, Stats};
use crate::store::WebhookRejection;
use crate::web::auth::{Auth, Session};
use crate::web::containers::Containers;
use crate::web::events::{RecentEvents, StackFeed};
use crate::web::pages::{Layout, Page, Section, Shell};
use crate::web::ui::Kpi;

impl Stacks {
    /// `GET /stacks/{name}`: one stack, with its containers and the deploy and remove forms.
    pub async fn show(
        Extension(stacks): Extension<Stacks>,
        Extension(stats): Extension<Stats>,
        Extension(containers): Extension<Containers>,
        shell: Shell,
        Path(name): Path<String>,
    ) -> Response {
        let layout = shell.layout(Section::Stacks);
        let status = match stacks.find(&name).await {
            Ok(status) => status,
            Err(outcome) => return outcome.page(layout),
        };
        let feed = StackFeed::new(stacks.clone(), &name);
        let (volumes, secrets, events, updates) = tokio::join!(
            stacks.0.volumes(&name),
            stacks.0.secrets(&name),
            feed.read(),
            stacks.0.stack_updates(&name)
        );
        // The card is left out when the updates cannot be read; the page still serves.
        let updates = updates
            .inspect_err(|err| {
                tracing::warn!(stack = %name, error = %err, "cannot read the stack's updates");
            })
            .ok()
            .and_then(|updates| UpdatesCard::new(&name, &updates, std::time::SystemTime::now()));
        let volumes = match volumes {
            Ok(volumes) => volumes,
            Err(err) => return Outcome::from(err).page(layout),
        };
        // Only the "not set" mark of a file's secret needs them; the environment tab reports
        // a store that cannot be read.
        let secrets = secrets.ok();
        let containers = containers
            .of_stack(&name)
            .await
            .map(StackContainers::from)
            .map_err(|err| {
                tracing::error!(stack = %name, error = %err, "listing the stack's containers failed");
                err.to_string()
            });
        Page(StackPage {
            layout,
            updates,
            header: stacks.header(&status, Tab::Overview).await,
            stack: StackView::new(
                &status,
                volumes,
                secrets.as_deref(),
                containers,
                events,
                &Readings::of(&stats).await,
            ),
        })
        .into_response()
    }

    /// `GET /stacks/{name}/events`: the overview's Recent events card as `text/event-stream`,
    /// sent again as the stack's events are recorded, and its containers' CPU and memory cells
    /// after each resource sampler tick, until the session ends.
    pub async fn events(
        State(auth): State<Arc<Auth>>,
        Extension(stacks): Extension<Stacks>,
        Extension(stats): Extension<Stats>,
        Extension(session): Extension<Session>,
        Path(name): Path<String>,
    ) -> Response {
        // Subscribed before the stack is looked up, so no event between the two is missed.
        let events = stacks.subscribe();
        let status = match stacks.find(&name).await {
            Ok(status) => status,
            Err(outcome) => return outcome.into_response(),
        };
        let usage = UsageFeed::new(&status, stats.resources().clone());
        StackFeed::new(stacks, &name)
            .with_usage(usage)
            .respond(events, auth, session)
    }
}

/// `GET /stacks/{name}`.
#[derive(Template)]
#[template(path = "stack.html")]
pub struct StackPage {
    layout: Layout,
    header: StackHeader,
    stack: StackView,
    /// The Updates card (ADR-0061 §9), when there is something on it.
    updates: Option<UpdatesCard>,
}

/// A stack's overview.
#[derive(Debug)]
pub struct StackView {
    name: String,
    url: String,
    /// The services, with the stack's containers.
    services: ServiceTable,
    /// Whether any service takes its digest from deploys, so the deploy form has fields.
    deployable: bool,
    /// Refused webhook attempts, newest first (ADR-0047).
    rejections: Vec<RejectionView>,
    /// `host` or `host/prefix` of every route of the active revision.
    routes: Vec<String>,
    /// The named volumes of every revision, which removing with `delete_volumes` deletes.
    volumes: Vec<VolumeView>,
    /// The config files of the active revision (ADR-0057), service by service.
    files: Vec<FileRow>,
    /// The KPI tiles.
    kpis: Vec<Kpi>,
    /// The responses by status over the [`TrafficWindow`].
    statuses: StatusChart,
    /// The routes of the active revision, for the routes card.
    route_rows: Vec<RouteRow>,
    /// The stack's newest events.
    events: RecentEvents,
    now: std::time::SystemTime,
}

impl StackView {
    pub(in crate::web) fn url(stack: &str) -> String {
        format!("/stacks/{stack}")
    }

    pub fn deploy_url(&self) -> String {
        format!("{}/deploy", self.url)
    }

    pub fn remove_url(&self) -> String {
        format!("{}/remove", self.url)
    }

    /// The stream that keeps the Recent events card live.
    pub fn events_url(&self) -> String {
        StackFeed::url(&self.name)
    }

    /// The services that take their digest from deploys.
    fn ci_built(&self) -> impl Iterator<Item = &ServiceRow> {
        self.services.rows.iter().filter(|s| s.ci_built)
    }

    /// When the page was rendered, for the uptimes' tooltips.
    fn now(&self) -> std::time::SystemTime {
        self.now
    }

    /// The range of traffic history the overview shows, for the chart's heading (`1h`).
    fn window(&self) -> Range {
        TrafficWindow::RANGE
    }

    /// The same in words, for the chart's tooltip (`last hour`).
    fn window_words(&self) -> TrafficWindow {
        TrafficWindow
    }

    /// The volumes the active revision mounts, for the volumes card.
    fn mounted(&self) -> impl Iterator<Item = &VolumeView> {
        self.volumes.iter().filter(|v| !v.mounts.is_empty())
    }

    /// Where a file's secret is set or replaced.
    fn environment_url(&self) -> String {
        format!("{}/environment", self.url)
    }

    /// How many volumes only earlier revisions declare.
    fn unmounted(&self) -> usize {
        self.volumes.iter().filter(|v| v.mounts.is_empty()).count()
    }

    /// The services' names, for the danger zone's "one container per service".
    fn service_names(&self) -> String {
        self.services
            .rows
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl StackView {
    /// The overview of `status`, whose revisions declare `volumes` (podman names), with its
    /// `containers` (or why they cannot be listed), `events` as its Recent events card and the
    /// figures of `readings`. `secrets` are the stack's, when the store could be read, for the
    /// "not set" mark of a config file's secret.
    fn new(
        status: &StackStatus,
        volumes: BTreeSet<String>,
        secrets: Option<&[StackSecret]>,
        containers: Result<StackContainers, String>,
        events: RecentEvents,
        readings: &Readings,
    ) -> Self {
        let stack = status.revision.toml.parse::<Stack>().ok();
        let usage = readings.resources.stack(&status.name);
        let services = ServiceTable::new(status, stack.as_ref(), containers, usage, readings.now);
        let figures = Figures::new(status, stack.as_ref(), readings);
        Self {
            now: readings.now,
            kpis: figures.kpis,
            statuses: figures.statuses,
            route_rows: figures.routes,
            events,
            url: Self::url(&status.name),
            name: status.name.clone(),
            deployable: services.rows.iter().any(|s| s.ci_built),
            services,
            rejections: status.rejections.iter().map(RejectionView::from).collect(),
            routes: StackCard::routes(status),
            volumes: VolumeView::all(status, volumes),
            files: stack
                .as_ref()
                .map(|stack| FileRow::all(stack, secrets))
                .unwrap_or_default(),
        }
    }
}

/// One config file of the active revision (ADR-0057 §10): where it goes, who may read it and
/// where its content comes from. The content itself is never part of the page.
#[derive(Debug, PartialEq, Eq)]
struct FileRow {
    service: String,
    path: String,
    /// Four octal digits, the default resolved.
    mode: String,
    /// `uid:gid`, or "container user" for a secret file without `owner`.
    owner: String,
    content: FileContent,
}

/// Where a config file's content comes from.
#[derive(Debug, PartialEq, Eq)]
enum FileContent {
    /// Text in the stack file, by its size.
    Inline { size: String },
    Secret {
        /// The secret's full name, `<stack>/<name>`.
        full: String,
        /// Whether it is set; `None` when the secret store could not be read.
        set: Option<bool>,
    },
}

impl FileRow {
    /// Every config file of `stack`, by service and path. `secrets` are the stack's, as
    /// [`crate::orchestrator::Orchestrator::secrets`] lists them.
    fn all(stack: &Stack, secrets: Option<&[StackSecret]>) -> Vec<Self> {
        let prefix = format!("{}/", stack.name);
        stack
            .services
            .iter()
            .flat_map(|(service, spec)| {
                let prefix = &prefix;
                spec.files.iter().map(move |(path, file)| {
                    Self::new(service.as_ref(), path, file, prefix, secrets)
                })
            })
            .collect()
    }

    fn new(
        service: &str,
        path: &ContainerPath,
        file: &ConfigFile,
        prefix: &str,
        secrets: Option<&[StackSecret]>,
    ) -> Self {
        let content = match &file.source {
            FileSource::Inline(content) => FileContent::Inline {
                size: FileSize(content.len()).to_string(),
            },
            FileSource::Secret(full) => {
                let name = full.strip_prefix(prefix).unwrap_or(full);
                FileContent::Secret {
                    full: full.clone(),
                    set: secrets.map(|secrets| {
                        secrets
                            .iter()
                            .any(|s| s.name == name && s.updated_at.is_some())
                    }),
                }
            }
        };
        Self {
            service: service.to_owned(),
            path: path.to_string(),
            mode: file.resolved_mode().to_string(),
            owner: match file.ownership() {
                Ownership::Ids(owner) => owner.to_string(),
                Ownership::ContainerUser => "container user".to_owned(),
            },
            content,
        }
    }
}

/// A file's size as the files card shows it: `812 B`, `1.5 KiB`.
struct FileSize(usize);

impl std::fmt::Display for FileSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            bytes @ 0..1024 => write!(f, "{bytes} B"),
            // Tenths of a KiB, rounded up so a file never looks smaller than it is.
            bytes => {
                let tenths = (bytes * 10).div_ceil(1024);
                write!(f, "{}.{} KiB", tenths / 10, tenths % 10)
            }
        }
    }
}

/// A named volume of the stack: its podman name, and where the active revision mounts it.
#[derive(Debug, PartialEq, Eq)]
struct VolumeView {
    /// `hoster-<stack>-<volume>` (ADR-0030).
    name: String,
    /// `service:/path`, for every service of the active revision that mounts it; empty for a
    /// volume only an earlier revision declared.
    mounts: Vec<String>,
}

impl VolumeView {
    /// The volumes `status`'s active revision mounts and the `others` earlier revisions
    /// declared, by name. The active revision adds none when it does not parse (it did when it
    /// was applied, so only a newer hoster could refuse it).
    fn all(status: &StackStatus, others: BTreeSet<String>) -> Vec<Self> {
        let mut volumes: BTreeMap<String, Vec<String>> =
            others.into_iter().map(|name| (name, Vec::new())).collect();
        if let Ok(stack) = status.revision.toml.parse::<Stack>() {
            for (service, mount) in VolumeMount::of(&stack) {
                volumes
                    .entry(mount.volume().to_owned())
                    .or_default()
                    .push(format!("{service}:{}", mount.target()));
            }
        }
        volumes
            .into_iter()
            .map(|(name, mounts)| VolumeView { name, mounts })
            .collect()
    }
}

/// A refused webhook attempt, with the claims that help write the `[deploy]` rule.
#[derive(Debug)]
struct RejectionView {
    at: String,
    reason: String,
    repository: String,
    repository_id: String,
    git_ref: String,
    environment: String,
    /// The `workflow_ref` claim (ADR-0047).
    workflow_ref: String,
    /// The `job_workflow_ref` claim, the value a `[deploy]` rule names, when it differs from
    /// `workflow_ref` (a reusable workflow).
    job_workflow_ref: Option<String>,
    actor: String,
    run_id: String,
}

impl From<&WebhookRejection> for RejectionView {
    fn from(rejection: &WebhookRejection) -> Self {
        Self {
            at: When(rejection.at).to_string(),
            reason: rejection.reason.clone(),
            repository: rejection.repository.clone(),
            repository_id: rejection.repository_id.clone(),
            git_ref: rejection.git_ref.clone(),
            environment: rejection.environment.clone().unwrap_or_default(),
            workflow_ref: rejection.workflow_ref.clone().unwrap_or_default(),
            job_workflow_ref: rejection
                .job_workflow_ref
                .clone()
                .filter(|job| Some(job) != rejection.workflow_ref.as_ref()),
            actor: rejection.actor.clone(),
            run_id: rejection.run_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::stacks::tests::{NOTES, notes_status, revision};

    #[test]
    fn volumes_are_listed_once_with_every_mount() {
        let toml = format!(
            "{NOTES}\n[services.api.volumes]\ndata = \"/data\"\n\n[services.storage.volumes]\ndata = \"/srv\"\ncache = \"/cache\"\n"
        );
        let mut status = notes_status();
        status.revision = revision("notes", 1, &toml);
        let earlier = BTreeSet::from(["hoster-notes-cache".into(), "hoster-notes-old".into()]);
        assert_eq!(
            VolumeView::all(&status, earlier),
            [
                VolumeView {
                    name: "hoster-notes-cache".into(),
                    mounts: vec!["storage:/cache".into()],
                },
                VolumeView {
                    name: "hoster-notes-data".into(),
                    mounts: vec!["api:/data".into(), "storage:/srv".into()],
                },
                VolumeView {
                    name: "hoster-notes-old".into(),
                    mounts: Vec::new(),
                },
            ]
        );
        assert!(VolumeView::all(&notes_status(), BTreeSet::new()).is_empty());
    }

    #[test]
    fn the_danger_zone_names_the_services_and_routes() {
        let view = StackView::new(
            &notes_status(),
            BTreeSet::new(),
            None,
            Ok(StackContainers::default()),
            RecentEvents::of_stack("notes", Some(Vec::new()), std::time::SystemTime::now()),
            &Readings::default(),
        );
        assert_eq!(view.service_names(), "api, storage");
        assert_eq!(view.ci_built().count(), 1);
        assert_eq!(view.routes, ["notes.example.com"]);
    }
}
