//! The frame every stack page shares (ADR-0054, epic 8bf): the `~/stacks/<name>[/tab]`
//! breadcrumb, the stack's name with its [`Condition`] pill, a mono line with the active revision
//! and the running, last and queued deployments, the header actions, and the tab bar.
//!
//! The actions are **Redeploy**, which redeploys the stack's latest deployment that went live
//! through the history's redeploy endpoint (a rollback to its digests and revision, ADR-0041);
//! when the latest deployment failed past it or one is in flight, the button names it and its
//! confirm says it rolls back or queues, and **Re-resolve tags** for stacks with third-party images (ADR-0025), whose preview lands
//! inline under the header. Deploying explicit digests is a form on the overview, not a header
//! action.
//!
//! The logs tab leads to the log page of one of the stack's containers until it gets a page of
//! its own (task gn6): `?service=` picks the service (the first by default) and `?container=`
//! one of its containers (the one running it by default), as the overview's services table
//! links them.
//!
//! A page builds a [`StackHeader`] with the [`Tab`] it is, and its template includes
//! `stack_header.html`, which reads the page's `header` field.

use std::fmt;
use std::time::SystemTime;

use axum::Extension;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::list::{Ago, Condition, LastDeploy, ServiceChip};
use super::overview::{StackContainers, StackView};
use super::{DeploymentView, Stacks, When};
use crate::orchestrator::{ServiceState, ServiceStatus, StackStatus};
use crate::store::{Deployment, DeploymentState};
use crate::web::containers::Containers;
use crate::web::pages::{Section, Shell};
use crate::web::ui::{Pill, Tone};

impl Stacks {
    /// The header of `status`'s `tab`, with the deployment "Redeploy" redeploys looked up.
    pub(super) async fn header(&self, status: &StackStatus, tab: Tab) -> StackHeader {
        let live = match status.last.as_ref() {
            Some(last) if last.state.is_redeployable() => Some(last.clone()),
            _ => match self.0.latest_live(&status.name).await {
                Ok(live) => live,
                Err(err) => {
                    tracing::warn!(stack = %status.name, error = %err, "looking up the deployment to redeploy failed");
                    None
                }
            },
        };
        StackHeader::new(status, tab, live.as_ref(), SystemTime::now())
    }

    /// `GET /stacks/{name}/logs[?service=…&container=…]`: the logs tab. Until it has a page of
    /// its own (task gn6), it leads to the log page of the container
    /// [`StackContainers::log_of`] picks, or back to the services table when the stack has no
    /// such container; an unknown stack is a 404.
    pub async fn logs(
        Extension(stacks): Extension<Stacks>,
        Extension(containers): Extension<Containers>,
        shell: Shell,
        Path(name): Path<String>,
        Query(query): Query<LogsQuery>,
    ) -> Response {
        let status = match stacks.find(&name).await {
            Ok(status) => status,
            Err(outcome) => return outcome.page(shell.layout(Section::Stacks)),
        };
        let containers = match containers.of_stack(&name).await {
            Ok(list) => StackContainers::from(list),
            Err(err) => {
                tracing::error!(stack = %name, error = %err, "listing the stack's containers failed");
                StackContainers::default()
            }
        };
        let target = containers.log_of(
            &status,
            query.service.as_deref(),
            query.container.as_deref(),
        );
        match target {
            Some(container) => Redirect::to(&format!("/containers/{container}/logs")),
            None => Redirect::to(&format!("{}#services", StackView::url(&name))),
        }
        .into_response()
    }
}

/// The query of the logs tab.
#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    service: Option<String>,
    container: Option<String>,
}

/// One tab of the stack pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Logs,
    Environment,
    Deployments,
}

impl Tab {
    /// Every tab, in the bar's order.
    const ALL: [Tab; 4] = [Tab::Overview, Tab::Logs, Tab::Environment, Tab::Deployments];

    /// The path below `/stacks/<name>`; empty for the overview.
    fn path(self) -> &'static str {
        match self {
            Tab::Overview => "",
            Tab::Logs => "/logs",
            Tab::Environment => "/secrets",
            Tab::Deployments => "/deployments",
        }
    }
}

impl fmt::Display for Tab {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tab::Overview => "overview",
            Tab::Logs => "logs",
            Tab::Environment => "environment",
            Tab::Deployments => "deployments",
        })
    }
}

/// A link of the tab bar.
#[derive(Debug)]
pub struct TabLink {
    pub label: Tab,
    pub href: String,
    pub current: bool,
}

/// The shared header of a stack's pages.
#[derive(Debug)]
pub struct StackHeader {
    pub(super) name: String,
    pub(super) url: String,
    pub(super) tab: Tab,
    pub(super) pill: Pill,
    pub(super) revision: u32,
    /// The active revision's content hash, shortened, and in full.
    pub(super) hash: String,
    pub(super) full_hash: String,
    /// When the active revision was applied: how long ago, and the time itself.
    pub(super) applied_ago: String,
    pub(super) applied_at: String,
    /// The deployment running now, as (id, state, url).
    pub(super) active: Option<(String, String, String)>,
    /// The latest deployment, unless it is the one running or queued.
    pub(super) last: Option<LastDeploy>,
    pub(super) queued: Option<String>,
    /// "revision N: deploy failed: …" (ADR-0049).
    pub(super) flag: Option<Pill>,
    /// What "Redeploy" redeploys: the latest deployment that went live.
    pub(super) redeploy: Option<RedeployAction>,
    /// Whether a service uses a third-party image, so "Re-resolve tags" has work to do.
    pub(super) third_party: bool,
}

impl StackHeader {
    /// The header of `status`'s `tab`. `live` is the stack's latest deployment that went live,
    /// the one "Redeploy" redeploys; without one there is no Redeploy button.
    pub fn new(status: &StackStatus, tab: Tab, live: Option<&Deployment>, now: SystemTime) -> Self {
        let condition = Condition::from(status);
        let hash = status.revision.hash.to_string();
        let active_id = status.active.map(|(id, _)| id);
        StackHeader {
            url: StackView::url(&status.name),
            name: status.name.clone(),
            tab,
            pill: Tone::from(condition).pill(format!("{condition} · {}", Up::of(&status.services))),
            revision: status.revision.revision,
            hash: hash.get(..12).unwrap_or(&hash).to_owned(),
            full_hash: hash.clone(),
            applied_ago: Ago::since(status.revision.created_at, now).to_string(),
            applied_at: When(status.revision.created_at).to_string(),
            active: status.active.map(|(id, state)| {
                (
                    id.to_string(),
                    state.to_string(),
                    DeploymentView::url(&status.name, id),
                )
            }),
            last: status
                .last
                .as_ref()
                .filter(|last| Some(last.id) != active_id && Some(last.id) != status.queued)
                .map(|last| LastDeploy::new(last, now)),
            queued: status.queued.map(|id| id.to_string()),
            flag: status.flag.as_ref().map(|flag| Tone::Fail.pill(flag)),
            redeploy: live.map(|deployment| RedeployAction::new(status, deployment)),
            third_party: status.services.iter().any(|s| !s.ci_built),
        }
    }

    /// The tabs, the current one marked.
    pub fn tabs(&self) -> Vec<TabLink> {
        Tab::ALL
            .into_iter()
            .map(|tab| TabLink {
                label: tab,
                href: format!("{}{}", self.url, tab.path()),
                current: tab == self.tab,
            })
            .collect()
    }

    /// The breadcrumb's last segment after the name, the tab as its bar names it (`/logs`,
    /// `/environment`, …); empty on the overview.
    pub fn crumb_tail(&self) -> String {
        match self.tab {
            Tab::Overview => String::new(),
            tab => format!("/{tab}"),
        }
    }

    pub fn reresolve_url(&self) -> String {
        format!("{}/reresolve/preview", self.url)
    }
}

/// The header's Redeploy button: the stack's latest deployment that went live.
#[derive(Debug)]
pub struct RedeployAction {
    pub(super) url: String,
    /// `Redeploy`, or `Redeploy #N` when the stack is not simply running it again.
    pub(super) label: String,
    pub(super) title: String,
    pub(super) confirm: String,
}

impl RedeployAction {
    /// Redeploying `live` of `status`'s stack. While a deployment runs or waits, the redeploy
    /// queues behind the running one and replaces the waiting one (ADR-0040); when the latest
    /// deployment failed, was rejected or superseded, it is a rollback past it. The label and
    /// confirm say which. A latest deployment that was a no-op leaves `live` running, so that is
    /// a plain redeploy.
    fn new(status: &StackStatus, live: &Deployment) -> Self {
        let id = live.id;
        let url = format!("{}/redeploy", DeploymentView::url(&status.name, id));
        let revision = if live.revision.is_none_or(|r| r == status.revision.id) {
            String::new()
        } else {
            format!(
                ", replacing the active revision {}",
                status.revision.revision
            )
        };
        let active = status.active.map(|(active, _)| active);
        if active.is_some() || status.queued.is_some() {
            let mut waits = Vec::new();
            if let Some(active) = active {
                waits.push(format!("queues behind the running deployment #{active}"));
            }
            if let Some(queued) = status.queued {
                waits.push(format!("replaces the queued deployment #{queued}"));
            }
            let waits = waits.join(" and ");
            return RedeployAction {
                url,
                label: format!("Redeploy #{id}"),
                title: format!("Redeploys #{id}, the latest that went live: it {waits}"),
                confirm: format!(
                    "Redeploy deployment #{id}? It {waits}; then #{id}'s digests and stack revision become the running ones{revision}."
                ),
            };
        }
        let rolled_past = status.last.as_ref().filter(|last| {
            last.id != id
                && matches!(
                    last.state,
                    DeploymentState::Failed
                        | DeploymentState::Rejected
                        | DeploymentState::Superseded
                        | DeploymentState::Aborting
                )
        });
        match rolled_past {
            None => RedeployAction {
                url,
                label: "Redeploy".to_owned(),
                title: format!("Redeploys deployment #{id}, the latest that went live"),
                confirm: format!(
                    "Redeploy deployment #{id}? Its digests and stack revision become the running ones again{revision}."
                ),
            },
            Some(latest) => {
                let latest = latest.id;
                RedeployAction {
                    url,
                    label: format!("Redeploy #{id}"),
                    title: format!(
                        "The latest deployment, #{latest}, is not live: this rolls back to #{id}, the latest that went live"
                    ),
                    confirm: format!(
                        "Roll back to deployment #{id}? The latest deployment, #{latest}, is not live; #{id}'s digests and stack revision become the running ones{revision}."
                    ),
                }
            }
        }
    }
}

/// How many services run and pass their checks, out of all: `2/3 up`.
struct Up {
    up: usize,
    total: usize,
}

impl Up {
    fn of(services: &[ServiceStatus]) -> Self {
        Up {
            up: services
                .iter()
                .filter(|s| {
                    matches!(s.state, ServiceState::Running { .. }) && !ServiceChip::failing(s)
                })
                .count(),
            total: services.len(),
        }
    }
}

impl fmt::Display for Up {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{} up", self.up, self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{DeploymentId, RevisionId};
    use crate::web::stacks::tests::{deployment, notes_status};

    #[test]
    fn the_crumb_names_the_tab_as_the_bar_does() {
        let status = notes_status();
        let crumb = |tab| StackHeader::new(&status, tab, None, SystemTime::now()).crumb_tail();
        assert_eq!(crumb(Tab::Overview), "");
        assert_eq!(crumb(Tab::Logs), "/logs");
        assert_eq!(crumb(Tab::Environment), "/environment");
        assert_eq!(crumb(Tab::Deployments), "/deployments");
    }

    #[test]
    fn redeploying_past_a_newer_revision_says_it_replaces_it() {
        let mut status = notes_status();
        status.last = Some(deployment(5, DeploymentState::Failed));
        let mut live = deployment(2, DeploymentState::Live);
        live.revision = Some(RevisionId::from(7));
        let redeploy = RedeployAction::new(&status, &live);
        assert_eq!(redeploy.label, "Redeploy #2");
        assert_eq!(redeploy.url, "/stacks/notes/deployments/2/redeploy");
        assert!(
            redeploy
                .confirm
                .ends_with("become the running ones, replacing the active revision 1."),
            "{}",
            redeploy.confirm
        );

        // The latest deployment itself: a plain redeploy.
        let redeploy = RedeployAction::new(&status, &deployment(5, DeploymentState::Live));
        assert_eq!(redeploy.label, "Redeploy");
    }

    #[test]
    fn a_noop_latest_deployment_is_a_plain_redeploy() {
        let mut status = notes_status();
        status.last = Some(deployment(5, DeploymentState::Noop));
        let redeploy = RedeployAction::new(&status, &deployment(3, DeploymentState::Live));
        assert_eq!(redeploy.label, "Redeploy");
        assert_eq!(
            redeploy.confirm,
            "Redeploy deployment #3? Its digests and stack revision become the running ones again."
        );
    }

    #[test]
    fn redeploying_during_a_deployment_says_it_queues() {
        let mut status = notes_status();
        status.last = Some(deployment(6, DeploymentState::Pulling));
        status.active = Some((DeploymentId::from(6), DeploymentState::Pulling));
        let live = deployment(3, DeploymentState::Live);
        let redeploy = RedeployAction::new(&status, &live);
        assert_eq!(redeploy.label, "Redeploy #3");
        assert_eq!(
            redeploy.confirm,
            "Redeploy deployment #3? It queues behind the running deployment #6; then #3's digests and stack revision become the running ones."
        );

        status.last = Some(deployment(7, DeploymentState::Queued));
        status.queued = Some(DeploymentId::from(7));
        let redeploy = RedeployAction::new(&status, &live);
        assert!(
            redeploy.confirm.starts_with(
                "Redeploy deployment #3? It queues behind the running deployment #6 and replaces the queued deployment #7;"
            ),
            "{}",
            redeploy.confirm
        );
        // The queued latest deployment shows once, as queued, not again as the last one.
        let header = StackHeader::new(&status, Tab::Overview, Some(&live), SystemTime::now());
        assert!(header.last.is_none());
    }
}
