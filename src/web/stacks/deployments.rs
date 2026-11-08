//! A stack's deployments tab, one deployment's page with its live progress, and "redeploy this
//! one" (ADR-0021, ADR-0040, ADR-0041, ADR-0047, architecture §3.3 #5).
//!
//! `GET /stacks/{name}/deployments` is the deployments tab: the newest deployments under the
//! stack's header, each with its [`Status`] pill, trigger (with the commit and repository of a
//! webhook deploy), target digests, start, duration and why it failed. Every one that went live
//! has a ghost [`Redeploy`] button (`POST …/deployments/{id}/redeploy`): a rollback to its
//! digests and stack revision, which sends the browser to the new deployment. It reads
//! "redeploy" on the newest deployment that went live and "roll back" on the older ones. Like
//! every form it posts with HTMX and carries the CSRF token in the `X-CSRF-Token` header.
//!
//! `GET …/deployments/{id}` shows one deployment under the same header, its breadcrumb ending in
//! `/deployments/<id>`: state and duration, cancel (before `Switching`, ADR-0040) and redeploy,
//! the failure, the target digests, the webhook's provenance and the transitions. A running
//! deployment's page follows it over SSE (`GET …/deployments/{id}/events`): the stream sends the
//! status partial at once, again whenever the orchestrator reports the deployment moved on
//! `ui_events` (or the receiver lagged), and ends with an `end` event once the deployment is
//! over, which closes the connection. `GET …/deployments/{id}/status` is the same partial.

use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use askama::Template;
use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream, StreamExt};
use tokio::sync::broadcast;

use super::frame::{StackHeader, Tab};
use super::list::{Ago, Commit, Source};
use super::{DeployForm, Outcome, ShortDigest, Stacks, When};
use crate::orchestrator::{self, OrchestratorError, UiEvent};
use crate::stats::Elapsed;
use crate::store::{Deployment, DeploymentEvent, DeploymentId, DeploymentState};
use crate::web::auth::{Auth, Session};
use crate::web::pages::{Layout, Page, Section, Shell};
use crate::web::ui::{Pill, Tone};

impl Stacks {
    /// How many deployments the history page shows.
    const HISTORY: u32 = 50;

    /// `GET /stacks/{name}/deployments`: the stack's newest deployments.
    pub async fn history(
        Extension(stacks): Extension<Stacks>,
        shell: Shell,
        Path(name): Path<String>,
    ) -> Response {
        let layout = shell.layout(Section::Stacks);
        let status = match stacks.find(&name).await {
            Ok(status) => status,
            Err(outcome) => return outcome.page(layout),
        };
        let deployments = match stacks.0.history(&name, Self::HISTORY).await {
            Ok(deployments) => deployments,
            Err(err) => return Outcome::from(err).page(layout),
        };
        let now = SystemTime::now();
        // Newest first, so the first that went live is the latest one.
        let latest_live = deployments
            .iter()
            .find(|d| d.state.is_redeployable())
            .map(|d| d.id);
        Page(HistoryPage {
            layout,
            header: stacks.header(&status, Tab::Deployments).await,
            limit: Self::HISTORY,
            deployments: deployments
                .iter()
                .map(|d| HistoryRow::new(d, latest_live, now))
                .collect(),
        })
        .into_response()
    }

    /// `GET /stacks/{name}/deployments/{id}`: one deployment, followed until it ends.
    pub async fn deployment_page(
        Extension(stacks): Extension<Stacks>,
        shell: Shell,
        Path((name, id)): Path<(String, String)>,
    ) -> Response {
        let layout = shell.layout(Section::Stacks);
        let deployment = match stacks.lookup(&name, &id).await {
            Ok(deployment) => deployment,
            Err(outcome) => return outcome.page(layout),
        };
        // A removed stack keeps its deployment history, but has no header to show.
        let status = match stacks.status().await {
            Ok(all) => all.into_iter().find(|s| s.name == name),
            Err(err) => {
                tracing::warn!(stack = %name, error = %err, "looking up the stack failed");
                None
            }
        };
        let header = match status {
            Some(status) => Some(
                stacks
                    .header(&status, Tab::Deployments)
                    .await
                    .at_deployment(&deployment.id),
            ),
            None => None,
        };
        Page(DeploymentPage {
            layout,
            header,
            deployment,
        })
        .into_response()
    }

    /// `GET /stacks/{name}/deployments/{id}/status`: the deployment's status partial, the
    /// one the page's live stream sends.
    pub async fn deployment_status(
        Extension(stacks): Extension<Stacks>,
        Path((name, id)): Path<(String, String)>,
    ) -> Response {
        match stacks.lookup(&name, &id).await {
            Ok(deployment) => Page(DeploymentStatus { deployment }).into_response(),
            Err(outcome) => outcome.into_response(),
        }
    }

    /// `POST /stacks/{name}/deployments/{id}/cancel`: cancels the deployment if it has not
    /// reached `Switching` (ADR-0040). The page's status partial follows the abort.
    pub async fn cancel(
        Extension(stacks): Extension<Stacks>,
        Path((name, id)): Path<(String, String)>,
    ) -> Response {
        let id = match DeploymentView::parse_id(&id) {
            Ok(id) => id,
            Err(outcome) => return outcome.into_response(),
        };
        match stacks.0.cancel(&name, id).await {
            Ok(()) => Outcome::success(format!(
                "Cancelling deployment {id}: it aborts and puts {name} back as it was."
            ))
            .into_response(),
            Err(err) => Outcome::from(err).into_response(),
        }
    }

    /// `POST /stacks/{name}/deployments/{id}/redeploy`: redeploys a deployment that went live,
    /// its digests and its stack revision (ADR-0041), and sends the browser to the new
    /// deployment.
    pub async fn redeploy(
        Extension(stacks): Extension<Stacks>,
        Path((name, id)): Path<(String, String)>,
    ) -> Response {
        let id = match DeploymentView::parse_id(&id) {
            Ok(id) => id,
            Err(outcome) => return outcome.into_response(),
        };
        let outcome = match stacks.0.rollback(&name, id).await {
            Ok(outcome) => outcome,
            Err(err) => return Outcome::from(err).into_response(),
        };
        let follow = outcome.state != DeploymentState::Noop;
        let mut response = DeployForm::outcome(&name, &outcome).into_response();
        if follow && let Ok(url) = DeploymentView::url(&name, outcome.deployment).parse() {
            response.headers_mut().insert("hx-redirect", url);
        }
        response
    }

    /// `GET /stacks/{name}/deployments/{id}/events`: the deployment's status partial as
    /// `text/event-stream`, sent again on every change until the deployment is over.
    pub async fn deployment_events(
        State(auth): State<Arc<Auth>>,
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
        Path((name, id)): Path<(String, String)>,
    ) -> Response {
        // Subscribed before the first read, so no change between the two is missed.
        let events = stacks.0.subscribe();
        let id = match DeploymentView::parse_id(&id) {
            Ok(id) => id,
            Err(outcome) => return outcome.into_response(),
        };
        if let Err(err) = stacks.0.deployment(&name, id).await {
            return Outcome::from(err).into_response();
        }
        let signed_out = async move {
            if let Err(err) = auth
                .session_ended(&session, DeploymentEvents::SESSION_CHECK)
                .await
            {
                tracing::error!(error = %err, "checking the session of a deployment stream failed");
            }
        };
        let progress = DeploymentEvents {
            stacks,
            stack: name,
            id,
        };
        Sse::new(progress.stream(events, signed_out))
            .keep_alive(KeepAlive::new().interval(DeploymentEvents::KEEP_ALIVE))
            .into_response()
    }

    /// Deployment `id` of `stack` (as the URL spells it) as its page shows it; an id that is
    /// not a number is a 404.
    async fn lookup(&self, stack: &str, id: &str) -> Result<DeploymentView, Outcome> {
        let id = DeploymentView::parse_id(id)?;
        Ok(self.view(stack, id).await?)
    }

    /// Deployment `id` of `stack` as its page shows it, with the stack's latest deployment that
    /// went live looked up, so its redeploy button says whether it rolls back.
    async fn view(
        &self,
        stack: &str,
        id: DeploymentId,
    ) -> Result<DeploymentView, OrchestratorError> {
        let (deployment, events) = self.0.deployment(stack, id).await?;
        let latest_live = if deployment.state.is_redeployable() {
            match self.0.latest_live(stack).await {
                Ok(live) => live.map(|d| d.id),
                // A removed stack keeps its history; it has nothing to roll back to.
                Err(OrchestratorError::UnknownStack(_)) => None,
                Err(err) => {
                    tracing::warn!(stack, error = %err, "looking up the latest live deployment failed");
                    None
                }
            }
        } else {
            None
        };
        Ok(DeploymentView::new(
            &deployment,
            &events,
            latest_live,
            SystemTime::now(),
        ))
    }
}

/// One deployment's progress as SSE events.
struct DeploymentEvents {
    stacks: Stacks,
    stack: String,
    id: DeploymentId,
}

/// What the progress stream does after sending an update.
enum Next {
    /// Waits for the deployment's next change.
    Wait(DeploymentEvents, broadcast::Receiver<UiEvent>),
    /// Sends the `end` event: the deployment is over, or cannot be followed any more.
    End,
}

impl DeploymentEvents {
    /// The named event after the last update; the page closes the connection on it.
    const END: &str = "end";
    /// How often an idle stream sends a comment, so a closed connection is noticed.
    const KEEP_ALIVE: Duration = Duration::from_secs(15);
    /// How often a stream checks that its session is still signed in.
    const SESSION_CHECK: Duration = if cfg!(test) {
        Duration::from_millis(20)
    } else {
        Duration::from_secs(30)
    };

    /// The current status, then one update per change of the deployment, then `end`. When
    /// `cut` resolves first (the session ended), the stream ends there with `end`.
    fn stream(
        self,
        events: broadcast::Receiver<UiEvent>,
        cut: impl Future<Output = ()> + Send + 'static,
    ) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
        let updates = stream::unfold(Some((self, events, true)), |state| async move {
            let (this, mut events, first) = state?;
            let mut open = true;
            if !first {
                open = this.changed(&mut events).await;
            }
            let (event, next) = this.update(events, open).await;
            let state = match next {
                Next::Wait(this, events) => Some((this, events, false)),
                Next::End => None,
            };
            Some((event, state))
        });
        updates
            .take_until(cut)
            .chain(stream::once(std::future::ready(
                Event::default().event(Self::END).data(""),
            )))
            .map(Ok)
    }

    /// Waits until the deployment changes, or the receiver lagged and may have missed a
    /// change. `false` once the orchestrator is gone: nothing changes any more.
    async fn changed(&self, events: &mut broadcast::Receiver<UiEvent>) -> bool {
        loop {
            match events.recv().await {
                Ok(UiEvent::Deployment { id, .. }) if id == self.id => return true,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => return true,
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    }

    /// The status partial as it is now, and whether to keep following.
    async fn update(self, events: broadcast::Receiver<UiEvent>, open: bool) -> (Event, Next) {
        let (html, finished) = match self.stacks.view(&self.stack, self.id).await {
            Ok(deployment) => {
                let finished = deployment.finished;
                (DeploymentStatus { deployment }.render(), finished)
            }
            Err(err) => (Outcome::from(err).render(), true),
        };
        let html = html.unwrap_or_else(|err| {
            tracing::error!(error = %err, "rendering a deployment's status failed");
            String::new()
        });
        let next = if finished || !open {
            Next::End
        } else {
            Next::Wait(self, events)
        };
        (Event::default().data(html), next)
    }
}

/// How a deployment reads at a glance, in the history and on its page: whether it went live,
/// failed, was cancelled, superseded or skipped, or is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Live,
    /// Live, but not everything went as planned (a route or a cleanup step failed).
    LiveWithWarnings,
    Failed,
    /// Refused before anything ran: the stack file or the request was invalid.
    Rejected,
    /// Cancelled from its page (ADR-0040): it failed with the orchestrator's
    /// [`orchestrator::CANCELLED`].
    Cancelled,
    /// A queued deployment a newer request replaced (ADR-0040).
    Superseded,
    /// Nothing to do: the digests and revision were already running.
    Skipped,
    /// Waiting behind the running deployment.
    Queued,
    /// Under way, in this state.
    Running(DeploymentState),
}

impl Status {
    /// The state behind a "running" pill, in words: `pulling`, `health checking`.
    fn phase(self) -> Option<Phase> {
        match self {
            Status::Running(state) => Some(Phase(state)),
            _ => None,
        }
    }

    fn tone(self) -> Tone {
        match self {
            Status::Live => Tone::Ok,
            Status::LiveWithWarnings => Tone::Warn,
            Status::Failed | Status::Rejected => Tone::Fail,
            Status::Cancelled | Status::Superseded | Status::Skipped => Tone::Neutral,
            Status::Queued | Status::Running(_) => Tone::Progress,
        }
    }

    /// What the pill's tooltip says.
    fn title(self) -> String {
        match self {
            Status::Live => "Went live".to_owned(),
            Status::LiveWithWarnings => "Went live, with warnings: see the transitions".to_owned(),
            Status::Failed => "Failed, and the stack was put back as it was".to_owned(),
            Status::Rejected => "Refused before anything was started".to_owned(),
            Status::Cancelled => "Cancelled, and the stack was put back as it was".to_owned(),
            Status::Superseded => "Replaced in the queue by a newer deployment".to_owned(),
            Status::Skipped => "Nothing to do: these digests and revision were running".to_owned(),
            Status::Queued => "Waiting for the running deployment to end".to_owned(),
            Status::Running(state) => format!("Running: {}", Phase(state)),
        }
    }
}

impl From<&Deployment> for Status {
    fn from(deployment: &Deployment) -> Self {
        use DeploymentState as S;
        match deployment.state {
            S::Live => Status::Live,
            S::LiveWithWarnings => Status::LiveWithWarnings,
            S::Failed if deployment.error.as_deref() == Some(orchestrator::CANCELLED) => {
                Status::Cancelled
            }
            S::Failed => Status::Failed,
            S::Rejected => Status::Rejected,
            S::Superseded => Status::Superseded,
            S::Noop => Status::Skipped,
            S::Queued => Status::Queued,
            state @ (S::Received
            | S::Validating
            | S::Pulling
            | S::Starting
            | S::HealthChecking
            | S::Replacing
            | S::Switching
            | S::Draining
            | S::CleaningUp
            | S::Aborting) => Status::Running(state),
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Live => "live",
            Status::LiveWithWarnings => "live · warnings",
            Status::Failed => "failed",
            Status::Rejected => "rejected",
            Status::Cancelled => "cancelled",
            Status::Superseded => "superseded",
            Status::Skipped => "skipped",
            Status::Queued => "queued",
            Status::Running(_) => "running",
        })
    }
}

impl From<Status> for Pill {
    fn from(status: Status) -> Self {
        status.tone().pill(status).title(status.title())
    }
}

/// A deployment state in lower-case words: `HealthChecking` is `health checking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Phase(pub(super) DeploymentState);

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in self.0.to_string().chars().enumerate() {
            if c.is_uppercase() && i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{}", c.to_lowercase())?;
        }
        Ok(())
    }
}

/// The redeploy button of a deployment that went live: "redeploy" on the stack's latest one,
/// "roll back" on an older one (ADR-0041).
#[derive(Debug)]
pub(super) struct Redeploy {
    pub(super) url: String,
    /// Whether it redeploys an older deployment than the latest live one.
    pub(super) rollback: bool,
    pub(super) confirm: String,
}

impl Redeploy {
    /// The button of `deployment`, if it went live; `latest_live` is the stack's newest
    /// deployment that did.
    fn of(deployment: &Deployment, latest_live: Option<DeploymentId>) -> Option<Self> {
        if !deployment.state.is_redeployable() {
            return None;
        }
        let id = deployment.id;
        let rollback = latest_live.is_some_and(|latest| latest != id);
        let confirm = if rollback {
            format!(
                "Roll back to deployment {id}? Its digests and stack revision become the running ones again."
            )
        } else {
            format!(
                "Redeploy deployment {id}? Its digests and stack revision become the running ones again."
            )
        };
        Some(Redeploy {
            url: format!(
                "{}/redeploy",
                DeploymentView::url(&deployment.stack, deployment.id)
            ),
            rollback,
            confirm,
        })
    }

    /// The button's text in the history table.
    pub(super) fn label(&self) -> &'static str {
        if self.rollback {
            "roll back"
        } else {
            "redeploy"
        }
    }

    /// The button's tooltip.
    pub(super) fn title(&self) -> &'static str {
        if self.rollback {
            "Redeploys this older deployment's pinned digests and stack revision"
        } else {
            "Redeploys the latest deployment that went live, its pinned digests and stack revision"
        }
    }
}

/// What started a deployment, as the history's trigger cell and the deployment page show it:
/// `manual`, `webhook`, `poller`, `apply`, `rollback`, and a webhook deploy's commit.
#[derive(Debug)]
pub(super) struct Trigger {
    pub(super) source: String,
    /// A webhook deploy's commit.
    pub(super) commit: Option<Commit>,
}

impl From<&Deployment> for Trigger {
    fn from(deployment: &Deployment) -> Self {
        Trigger {
            source: Source(deployment.source).to_string(),
            commit: Commit::of(deployment),
        }
    }
}

/// One deployment in the history table.
#[derive(Debug)]
struct HistoryRow {
    id: String,
    url: String,
    status: Pill,
    phase: Option<Phase>,
    trigger: Trigger,
    /// Target digests as (service, short digest, digest).
    digests: Vec<(String, String, String)>,
    /// When it started: how long ago, and the time itself.
    started_ago: String,
    started_at: String,
    duration: String,
    error: Option<String>,
    redeploy: Option<Redeploy>,
}

impl HistoryRow {
    fn new(deployment: &Deployment, latest_live: Option<DeploymentId>, now: SystemTime) -> Self {
        let status = Status::from(deployment);
        Self {
            id: deployment.id.to_string(),
            url: DeploymentView::url(&deployment.stack, deployment.id),
            status: Pill::from(status),
            phase: status.phase(),
            trigger: Trigger::from(deployment),
            digests: deployment
                .target
                .iter()
                .map(|(service, digest)| {
                    (
                        service.clone(),
                        ShortDigest(digest).to_string(),
                        digest.clone(),
                    )
                })
                .collect(),
            started_ago: Ago::since(deployment.created_at, now).to_string(),
            started_at: When(deployment.created_at).to_string(),
            duration: Elapsed(deployment.duration()).to_string(),
            // A cancelled deployment's reason is its pill.
            error: deployment
                .error
                .clone()
                .filter(|_| status != Status::Cancelled),
            redeploy: Redeploy::of(deployment, latest_live),
        }
    }
}

/// A deployment with its transitions, as its page shows it.
#[derive(Debug)]
pub struct DeploymentView {
    id: String,
    stack: String,
    url: String,
    status: Pill,
    /// The exact state, as the transitions name it.
    state: String,
    phase: Option<Phase>,
    trigger: Trigger,
    generation: Option<u32>,
    error: Option<String>,
    target: Vec<(String, String)>,
    /// Where a webhook deployment came from, as (label, value) rows.
    provenance: Vec<(&'static str, String)>,
    events: Vec<EventView>,
    created_at: String,
    started_ago: String,
    finished_at: Option<String>,
    /// Whether it is over; a running one is followed over SSE.
    finished: bool,
    /// Whether it may still be cancelled: active and before `Switching` (ADR-0040).
    cancellable: bool,
    /// Set when it went live, so it can be redeployed as a rollback (ADR-0041).
    redeploy: Option<Redeploy>,
    /// How long it took, or has been running.
    duration: String,
}

impl DeploymentView {
    pub(in crate::web) fn url(stack: &str, id: DeploymentId) -> String {
        format!("/stacks/{stack}/deployments/{id}")
    }

    pub(super) fn parse_id(id: &str) -> Result<DeploymentId, Outcome> {
        id.parse()
            .map_err(|_| Outcome::error(StatusCode::NOT_FOUND, format!("No deployment {id}.")))
    }

    fn new(
        deployment: &Deployment,
        events: &[DeploymentEvent],
        latest_live: Option<DeploymentId>,
        now: SystemTime,
    ) -> Self {
        let status = Status::from(deployment);
        let url = Self::url(&deployment.stack, deployment.id);
        Self {
            id: deployment.id.to_string(),
            stack: deployment.stack.clone(),
            status: Pill::from(status),
            state: deployment.state.to_string(),
            phase: status.phase(),
            trigger: Trigger::from(deployment),
            generation: deployment.generation,
            error: deployment.error.clone(),
            target: deployment
                .target
                .iter()
                .map(|(service, digest)| (service.clone(), digest.clone()))
                .collect(),
            provenance: deployment
                .provenance
                .as_ref()
                .map(|p| {
                    let mut rows = vec![
                        ("repository", p.repository.clone()),
                        ("ref", p.git_ref.clone()),
                        ("commit", p.sha.clone()),
                        ("run", p.run_id.clone()),
                        ("actor", p.actor.clone()),
                    ];
                    if let Some(tag) = &p.tag {
                        rows.push(("tag", tag.clone()));
                    }
                    rows
                })
                .unwrap_or_default(),
            events: events.iter().map(EventView::from).collect(),
            created_at: When(deployment.created_at).to_string(),
            started_ago: Ago::since(deployment.created_at, now).to_string(),
            finished_at: deployment.finished_at.map(|t| When(t).to_string()),
            finished: deployment.state.is_terminal(),
            cancellable: deployment.state.is_cancellable(),
            redeploy: Redeploy::of(deployment, latest_live),
            duration: Elapsed(deployment.duration()).to_string(),
            url,
        }
    }

    /// The live progress stream (SSE) the page follows while the deployment runs.
    pub fn events_url(&self) -> String {
        format!("{}/events", self.url)
    }

    pub fn cancel_url(&self) -> String {
        format!("{}/cancel", self.url)
    }
}

/// One state transition.
#[derive(Debug)]
struct EventView {
    from: String,
    to: String,
    message: String,
    at: String,
}

impl From<&DeploymentEvent> for EventView {
    fn from(event: &DeploymentEvent) -> Self {
        Self {
            from: event.from.map(|s| s.to_string()).unwrap_or_default(),
            to: event.to.to_string(),
            message: event.message.clone().unwrap_or_default(),
            at: When(event.at).to_string(),
        }
    }
}

/// `GET /stacks/{name}/deployments`.
#[derive(Template)]
#[template(path = "deployments.html")]
pub struct HistoryPage {
    layout: Layout,
    header: StackHeader,
    /// How many deployments the table shows at most.
    limit: u32,
    deployments: Vec<HistoryRow>,
}

/// `GET /stacks/{name}/deployments/{id}`.
#[derive(Template)]
#[template(path = "deployment.html")]
pub struct DeploymentPage {
    layout: Layout,
    /// The stack's header; `None` once the stack was removed.
    header: Option<StackHeader>,
    deployment: DeploymentView,
}

/// `GET /stacks/{name}/deployments/{id}/status`.
#[derive(Template)]
#[template(path = "deployment_status.html")]
pub struct DeploymentStatus {
    deployment: DeploymentView,
}

#[cfg(test)]
mod tests;
