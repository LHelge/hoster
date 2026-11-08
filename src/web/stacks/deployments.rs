//! A stack's deployment history, live deployment progress and "redeploy this one" (ADR-0021,
//! ADR-0040, ADR-0041, architecture §3.3 #5).
//!
//! `GET /stacks/{name}/deployments` lists the newest deployments with their state, source,
//! target digests, duration and failure reason. Every one that went live has a "Redeploy" button
//! (`POST …/deployments/{id}/redeploy`): a rollback to its digests and stack revision, which
//! sends the browser to the new deployment. Like every form it posts with HTMX and carries the
//! CSRF token in the `X-CSRF-Token` header.
//!
//! A running deployment's page follows it over SSE (`GET …/deployments/{id}/events`): the
//! stream sends the status partial at once, again whenever the orchestrator reports the
//! deployment moved on `ui_events` (or the receiver lagged), and ends with an `end` event once
//! the deployment is over, which closes the connection.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::Extension;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream, StreamExt};
use tokio::sync::broadcast;

use super::{
    DeployForm, DeploymentStatus, DeploymentView, Outcome, ShortDigest, StackView, Stacks, When,
};
use crate::orchestrator::UiEvent;
use crate::stats::Elapsed;
use crate::store::{Deployment, DeploymentId, DeploymentState};
use crate::web::auth::{Auth, Session};
use crate::web::pages::{Layout, Page, Section, Shell};
use crate::web::ui::Pill;

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
        match stacks.0.history(&name, Self::HISTORY).await {
            Ok(deployments) => Page(HistoryPage {
                layout,
                stack_url: StackView::url(&name),
                deployments: deployments.iter().map(HistoryRow::from).collect(),
                stack: name,
            })
            .into_response(),
            Err(err) => Outcome::from(err).page(layout),
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
        let (html, finished) = match self.stacks.0.deployment(&self.stack, self.id).await {
            Ok((deployment, log)) => {
                let view = DeploymentView::new(&deployment, &log);
                let finished = view.finished;
                (DeploymentStatus { deployment: view }.render(), finished)
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

/// One deployment in the history table.
#[derive(Debug)]
struct HistoryRow {
    id: String,
    url: String,
    state: Pill,
    source: String,
    /// Target digests as (service, short digest, digest).
    digests: Vec<(String, String, String)>,
    started: String,
    duration: String,
    finished: bool,
    error: Option<String>,
    /// Set when it went live, so it can be redeployed.
    redeploy_url: Option<String>,
}

impl From<&Deployment> for HistoryRow {
    fn from(deployment: &Deployment) -> Self {
        let url = DeploymentView::url(&deployment.stack, deployment.id);
        Self {
            id: deployment.id.to_string(),
            state: Pill::from(deployment.state),
            source: deployment.source.to_string(),
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
            started: When(deployment.created_at).to_string(),
            duration: Elapsed(deployment.duration()).to_string(),
            finished: deployment.state.is_terminal(),
            error: deployment.error.clone(),
            redeploy_url: deployment
                .state
                .is_redeployable()
                .then(|| format!("{url}/redeploy")),
            url,
        }
    }
}

/// `GET /stacks/{name}/deployments`.
#[derive(Template)]
#[template(path = "deployments.html")]
pub struct HistoryPage {
    layout: Layout,
    stack: String,
    stack_url: String,
    deployments: Vec<HistoryRow>,
}

#[cfg(test)]
mod tests;
