//! `GET /stacks/{name}`: a stack's overview tab (ADR-0054, epic 8bf), under the shared
//! [`StackHeader`].
//!
//! The overview opens with the stack's KPI tiles and its slowest routes, from hoster's
//! in-process statistics (ADR-0045), then shows the services with their containers, the routes
//! and named volumes of the active revision, the form that deploys
//! explicit digests to the CI-built services, a link to apply a new revision, the refused
//! webhook deploys (ADR-0047) and, last, the danger zone: removing the stack once its name is
//! typed (ADR-0030). The danger zone says what goes (containers, network, routes) and what stays
//! (revisions, history, and the volumes unless they are ticked for deletion), and lists the
//! volumes ticking deletes: those of every revision, as [`Orchestrator::volumes`] finds them,
//! with where the active revision mounts each.
//!
//! The tiles, the slowest routes and the routes card are built in `figures.rs`; the services
//! table, which lists the stack's containers too, in `services.rs`.
//!
//! [`Orchestrator::volumes`]: crate::orchestrator::Orchestrator::volumes

mod figures;
mod services;

use std::collections::{BTreeMap, BTreeSet};

use askama::Template;
use axum::Extension;
use axum::extract::Path;
use axum::response::{IntoResponse, Response};

use self::figures::{Figures, Readings, RouteRow, SlowRoute};
#[cfg(test)]
pub(super) use self::services::Role;
pub(super) use self::services::{LogCandidate, LogPick, StackContainers};
use self::services::{ServiceRow, ServiceTable};
use super::frame::{StackHeader, Tab};
use super::list::StackCard;
use super::{Outcome, Stacks, When};
use crate::orchestrator::StackStatus;
use crate::runtime::VolumeMount;
use crate::stack::Stack;
use crate::stats::{Elapsed, Stats};
use crate::store::WebhookRejection;
use crate::web::containers::Containers;
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
        let volumes = match stacks.0.volumes(&name).await {
            Ok(volumes) => volumes,
            Err(err) => return Outcome::from(err).page(layout),
        };
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
            header: stacks.header(&status, Tab::Overview).await,
            stack: StackView::new(&status, volumes, containers, &Readings::of(&stats).await),
        })
        .into_response()
    }
}

/// `GET /stacks/{name}`.
#[derive(Template)]
#[template(path = "stack.html")]
pub struct StackPage {
    layout: Layout,
    header: StackHeader,
    stack: StackView,
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
    /// The KPI tiles.
    kpis: Vec<Kpi>,
    /// The routes the proxy has counted, slowest first.
    slowest: Vec<SlowRoute>,
    /// The routes of the active revision, for the routes card.
    route_rows: Vec<RouteRow>,
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

    /// The services that take their digest from deploys.
    fn ci_built(&self) -> impl Iterator<Item = &ServiceRow> {
        self.services.rows.iter().filter(|s| s.ci_built)
    }

    /// When the page was rendered, for the uptimes' tooltips.
    fn now(&self) -> std::time::SystemTime {
        self.now
    }

    /// How far back the latency percentiles look.
    fn latency_window(&self) -> Elapsed {
        Elapsed(Stats::LATENCY_WINDOW)
    }

    /// The volumes the active revision mounts, for the volumes card.
    fn mounted(&self) -> impl Iterator<Item = &VolumeView> {
        self.volumes.iter().filter(|v| !v.mounts.is_empty())
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
    /// `containers` (or why they cannot be listed) and the figures of `readings`.
    fn new(
        status: &StackStatus,
        volumes: BTreeSet<String>,
        containers: Result<StackContainers, String>,
        readings: &Readings,
    ) -> Self {
        let stack = status.revision.toml.parse::<Stack>().ok();
        let services = ServiceTable::new(status, stack.as_ref(), containers, readings.now);
        let figures = Figures::new(status, stack.as_ref(), readings);
        Self {
            now: readings.now,
            kpis: figures.kpis,
            slowest: figures.slowest,
            route_rows: figures.routes,
            url: Self::url(&status.name),
            name: status.name.clone(),
            deployable: services.rows.iter().any(|s| s.ci_built),
            services,
            rejections: status.rejections.iter().map(RejectionView::from).collect(),
            routes: StackCard::routes(status),
            volumes: VolumeView::all(status, volumes),
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
            Ok(StackContainers::default()),
            &Readings::default(),
        );
        assert_eq!(view.service_names(), "api, storage");
        assert_eq!(view.ci_built().count(), 1);
        assert_eq!(view.routes, ["notes.example.com"]);
    }
}
