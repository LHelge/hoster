//! `GET /stacks/{name}`: a stack's overview tab (ADR-0054, epic 8bf), under the shared
//! [`StackHeader`].
//!
//! The overview opens with the stack's KPI tiles and its slowest routes, from hoster's
//! in-process statistics (ADR-0045), then shows the services with their images and digests, the
//! routes and named volumes of the active revision, the form that deploys
//! explicit digests to the CI-built services, a link to apply a new revision, the refused
//! webhook deploys (ADR-0047) and, last, the danger zone: removing the stack once its name is
//! typed (ADR-0030). The danger zone says what goes (containers, network, routes) and what stays
//! (revisions, history, and the volumes unless they are ticked for deletion), and lists the
//! volumes ticking deletes: those of every revision, as [`Orchestrator::volumes`] finds them,
//! with where the active revision mounts each.
//!
//! The tiles, the slowest routes and the routes card are built in `figures.rs`.
//!
//! [`Orchestrator::volumes`]: crate::orchestrator::Orchestrator::volumes

mod figures;

use std::collections::{BTreeMap, BTreeSet};

use askama::Template;
use axum::Extension;
use axum::extract::Path;
use axum::response::{IntoResponse, Response};

use self::figures::{Figures, Readings, RouteRow, SlowRoute};
use super::frame::{StackHeader, Tab};
use super::list::StackCard;
use super::{Outcome, ShortDigest, Stacks, When};
use crate::orchestrator::{ServiceState, ServiceStatus, StackStatus};
use crate::runtime::VolumeMount;
use crate::stack::Stack;
use crate::stats::{Elapsed, Stats};
use crate::store::WebhookRejection;
use crate::web::pages::{Layout, Page, Section, Shell};
use crate::web::ui::Kpi;

impl Stacks {
    /// `GET /stacks/{name}`: one stack, with the deploy and remove forms.
    pub async fn show(
        Extension(stacks): Extension<Stacks>,
        Extension(stats): Extension<Stats>,
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
        Page(StackPage {
            layout,
            header: stacks.header(&status, Tab::Overview).await,
            stack: StackView::new(&status, volumes, &Readings::of(&stats).await),
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
    services: Vec<ServiceView>,
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
    fn ci_built(&self) -> impl Iterator<Item = &ServiceView> {
        self.services.iter().filter(|s| s.ci_built)
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
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl StackView {
    /// The overview of `status`, whose revisions declare `volumes` (podman names), with the
    /// figures of `readings`.
    fn new(status: &StackStatus, volumes: BTreeSet<String>, readings: &Readings) -> Self {
        let services: Vec<ServiceView> = status.services.iter().map(ServiceView::from).collect();
        let stack = status.revision.toml.parse::<Stack>().ok();
        let figures = Figures::new(status, stack.as_ref(), readings);
        Self {
            kpis: figures.kpis,
            slowest: figures.slowest,
            route_rows: figures.routes,
            url: Self::url(&status.name),
            name: status.name.clone(),
            deployable: services.iter().any(|s| s.ci_built),
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

/// A service as the overview shows it.
#[derive(Debug)]
struct ServiceView {
    name: String,
    image: String,
    ci_built: bool,
    /// What it runs, in words.
    state: String,
    digest: Option<String>,
    short_digest: Option<String>,
}

impl From<&ServiceStatus> for ServiceView {
    fn from(service: &ServiceStatus) -> Self {
        let (state, digest) = match &service.state {
            ServiceState::Running { generation, digest } => {
                (format!("generation {generation}"), Some(digest.clone()))
            }
            ServiceState::AwaitingFirstDigest => ("awaiting its first digest".to_owned(), None),
            ServiceState::NotDeployed => ("not deployed".to_owned(), None),
        };
        Self {
            name: service.name.clone(),
            image: service.image.clone(),
            ci_built: service.ci_built,
            state,
            short_digest: digest.as_deref().map(|d| ShortDigest(d).to_string()),
            digest,
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
        let view = StackView::new(&notes_status(), BTreeSet::new(), &Readings::default());
        assert_eq!(view.service_names(), "api, storage");
        assert_eq!(view.ci_built().count(), 1);
        assert_eq!(view.routes, ["notes.example.com"]);
    }
}
