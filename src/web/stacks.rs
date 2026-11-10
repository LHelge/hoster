//! The stack pages (ADR-0021, ADR-0027, ADR-0049): the stacks with their services, digests,
//! active revision and last deployment; applying a stack file; a manual deploy; removing a
//! stack.
//!
//! Every handler talks only to the orchestrator (ADR-0050), through [`StackControl`]. The
//! read-only pages are plain HTML and work without JavaScript. The forms post with HTMX (the
//! CSRF token travels in the `X-CSRF-Token` header that `base.html` adds to every HTMX request)
//! and get a partial back, which HTMX swaps into the form's result area.
//!
//! Applying is two steps. `POST /apply/preview` takes the pasted or uploaded file and answers
//! with its validation errors (with line numbers) or with what applying it would do: the diff
//! against the active revision, the digests third-party tags resolve to (ADR-0025) and the
//! services that will be redeployed. Its confirm form posts the same file to `POST /apply`,
//! which applies and redeploys in one step (there is no "apply without redeploy", ADR-0049) and
//! sends the browser to the resulting deployment's page, which follows it until it ends. The
//! page and its preview (the plan, the pre-flight checks and the diff) are in `apply.rs`.
//!
//! "Re-resolve tags" (ADR-0025) works the same way for the active revision: the preview shows
//! the digests every third-party tag resolves to now and the services that would move, and the
//! confirmation applies them as a new revision and redeploys (ADR-0049). The environment tab,
//! with the stack's variables and its write-only secrets (ADR-0029), is in `environment.rs`.
//! The list at `/stacks`, as cards with a status filter, is in `list.rs`; a stack's overview tab
//! is in `overview.rs`, under the header and tab bar every stack page shares (`frame.rs`), and
//! its logs tab in `logs.rs`. The deployment history, live progress and "redeploy this one" are
//! in `deployments.rs`.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use askama::Template;
use axum::Extension;
use axum::extract::{Form, FromRequest, Multipart, Path, Request};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::broadcast;

use super::auth::Session;
use super::pages::{FlashKind, Layout, Page, Section, Shell};
use crate::orchestrator::{
    ApplyOutcome, ApplyPreview, DeployOutcome, Operator, Orchestrator, OrchestratorError,
    RemoveOutcome, RemoveStack, SecretChange, StackSecret, StackStatus, UiEvent, WebhookDeploy,
};
use crate::runtime::ContainerRuntime;
use crate::secrets::{SecretError, SecretValue};
use crate::stack::{Counted, Diagnostic, StackError};
use crate::store::{
    Deployment, DeploymentEvent, DeploymentId, DeploymentSource, DeploymentState, DigestSet,
    EventPage, EventQuery, Timestamp, WebhookRejection,
};

pub(super) use crate::store::ShortDigest;

/// What the stack pages need from the orchestrator. Dyn-compatible, so the app holds the real
/// orchestrator (any runtime) or a fake in tests behind one `Arc`.
#[async_trait::async_trait]
pub trait StackControl: Send + Sync + 'static {
    /// Every stack; see [`Orchestrator::status`].
    async fn status(&self) -> Result<Vec<StackStatus>, OrchestratorError>;

    /// How many stacks there are, for the navigation; cheaper than [`StackControl::status`].
    async fn count(&self) -> Result<usize, OrchestratorError> {
        Ok(self.status().await?.len())
    }

    /// What applying `toml` would do; see [`Orchestrator::preview`].
    async fn preview(&self, toml: &str) -> Result<ApplyPreview, OrchestratorError>;

    /// Applies `toml` and redeploys what changes, done `by` the signed-in operator; see
    /// [`Orchestrator::apply`].
    async fn apply(&self, toml: &str, by: &Operator) -> Result<ApplyOutcome, OrchestratorError>;

    /// A manual deploy of `images` (CI-built services only); see [`Orchestrator::deploy`].
    async fn deploy(
        &self,
        stack: &str,
        images: DigestSet,
    ) -> Result<DeployOutcome, OrchestratorError>;

    /// A deploy from the webhook; see [`Orchestrator::deploy_webhook`].
    async fn deploy_webhook(
        &self,
        request: WebhookDeploy,
    ) -> Result<DeployOutcome, OrchestratorError>;

    /// See [`Orchestrator::record_rejection`].
    async fn record_rejection(&self, rejection: &WebhookRejection)
    -> Result<(), OrchestratorError>;

    /// See [`Orchestrator::remove`]; done `by` the signed-in operator.
    async fn remove(
        &self,
        request: RemoveStack,
        by: &Operator,
    ) -> Result<RemoveOutcome, OrchestratorError>;

    /// See [`Orchestrator::deployment`].
    async fn deployment(
        &self,
        stack: &str,
        id: DeploymentId,
    ) -> Result<(Deployment, Vec<DeploymentEvent>), OrchestratorError>;

    /// See [`Orchestrator::cancel`].
    async fn cancel(&self, stack: &str, id: DeploymentId) -> Result<(), OrchestratorError>;

    /// See [`Orchestrator::history`].
    async fn history(&self, stack: &str, limit: u32) -> Result<Vec<Deployment>, OrchestratorError>;

    /// See [`Orchestrator::latest_live`].
    async fn latest_live(&self, stack: &str) -> Result<Option<Deployment>, OrchestratorError>;

    /// See [`Orchestrator::volumes`].
    async fn volumes(&self, stack: &str) -> Result<BTreeSet<String>, OrchestratorError>;

    /// See [`Orchestrator::rollback`].
    async fn rollback(
        &self,
        stack: &str,
        id: DeploymentId,
    ) -> Result<DeployOutcome, OrchestratorError>;

    /// See [`Orchestrator::subscribe`].
    fn subscribe(&self) -> broadcast::Receiver<UiEvent>;

    /// One page of the event log (ADR-0055); see [`crate::orchestrator::EventLog::page`].
    async fn event_page(&self, query: &EventQuery) -> Result<EventPage, OrchestratorError>;

    /// See [`Orchestrator::reresolve_preview`].
    async fn reresolve_preview(&self, stack: &str) -> Result<ApplyPreview, OrchestratorError>;

    /// See [`Orchestrator::reresolve`]; done `by` the signed-in operator.
    async fn reresolve(
        &self,
        stack: &str,
        expected: u32,
        by: &Operator,
    ) -> Result<Option<ApplyOutcome>, OrchestratorError>;

    /// See [`Orchestrator::secrets`].
    async fn secrets(&self, stack: &str) -> Result<Vec<StackSecret>, OrchestratorError>;

    /// See [`Orchestrator::set_secret`]; done `by` the signed-in operator.
    async fn set_secret(
        &self,
        stack: &str,
        name: &str,
        value: &SecretValue,
        by: &Operator,
    ) -> Result<SecretChange, OrchestratorError>;

    /// See [`Orchestrator::delete_secret`]; done `by` the signed-in operator.
    async fn delete_secret(
        &self,
        stack: &str,
        name: &str,
        by: &Operator,
    ) -> Result<SecretChange, OrchestratorError>;
}

#[async_trait::async_trait]
impl<R: ContainerRuntime> StackControl for Arc<Orchestrator<R>> {
    async fn status(&self) -> Result<Vec<StackStatus>, OrchestratorError> {
        Orchestrator::status(self).await
    }

    async fn count(&self) -> Result<usize, OrchestratorError> {
        Orchestrator::stack_count(self).await
    }

    async fn preview(&self, toml: &str) -> Result<ApplyPreview, OrchestratorError> {
        Orchestrator::preview(self, toml).await
    }

    async fn apply(&self, toml: &str, by: &Operator) -> Result<ApplyOutcome, OrchestratorError> {
        self.by(by).apply(toml).await
    }

    async fn deploy(
        &self,
        stack: &str,
        images: DigestSet,
    ) -> Result<DeployOutcome, OrchestratorError> {
        Orchestrator::deploy(self, stack, images, DeploymentSource::Manual).await
    }

    async fn deploy_webhook(
        &self,
        request: WebhookDeploy,
    ) -> Result<DeployOutcome, OrchestratorError> {
        Orchestrator::deploy_webhook(self, request).await
    }

    async fn record_rejection(
        &self,
        rejection: &WebhookRejection,
    ) -> Result<(), OrchestratorError> {
        Orchestrator::record_rejection(self, rejection).await
    }

    async fn remove(
        &self,
        request: RemoveStack,
        by: &Operator,
    ) -> Result<RemoveOutcome, OrchestratorError> {
        self.by(by).remove(request).await
    }

    async fn deployment(
        &self,
        stack: &str,
        id: DeploymentId,
    ) -> Result<(Deployment, Vec<DeploymentEvent>), OrchestratorError> {
        Orchestrator::deployment(self, stack, id).await
    }

    async fn cancel(&self, stack: &str, id: DeploymentId) -> Result<(), OrchestratorError> {
        Orchestrator::cancel(self, stack, id).await
    }

    async fn history(&self, stack: &str, limit: u32) -> Result<Vec<Deployment>, OrchestratorError> {
        Orchestrator::history(self, stack, limit).await
    }

    async fn latest_live(&self, stack: &str) -> Result<Option<Deployment>, OrchestratorError> {
        Orchestrator::latest_live(self, stack).await
    }

    async fn volumes(&self, stack: &str) -> Result<BTreeSet<String>, OrchestratorError> {
        Orchestrator::volumes(self, stack).await
    }

    async fn rollback(
        &self,
        stack: &str,
        id: DeploymentId,
    ) -> Result<DeployOutcome, OrchestratorError> {
        Orchestrator::rollback(self, stack, id).await
    }

    fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        Orchestrator::subscribe(self)
    }

    async fn event_page(&self, query: &EventQuery) -> Result<EventPage, OrchestratorError> {
        Ok(self.event_log().page(query).await?)
    }

    async fn reresolve_preview(&self, stack: &str) -> Result<ApplyPreview, OrchestratorError> {
        Orchestrator::reresolve_preview(self, stack).await
    }

    async fn reresolve(
        &self,
        stack: &str,
        expected: u32,
        by: &Operator,
    ) -> Result<Option<ApplyOutcome>, OrchestratorError> {
        self.by(by).reresolve(stack, expected).await
    }

    async fn secrets(&self, stack: &str) -> Result<Vec<StackSecret>, OrchestratorError> {
        Orchestrator::secrets(self, stack).await
    }

    async fn set_secret(
        &self,
        stack: &str,
        name: &str,
        value: &SecretValue,
        by: &Operator,
    ) -> Result<SecretChange, OrchestratorError> {
        self.by(by).set_secret(stack, name, value).await
    }

    async fn delete_secret(
        &self,
        stack: &str,
        name: &str,
        by: &Operator,
    ) -> Result<SecretChange, OrchestratorError> {
        self.by(by).delete_secret(stack, name).await
    }
}

/// The stack pages' handlers, sharing the orchestrator as a request extension.
#[derive(Clone)]
pub struct Stacks(Arc<dyn StackControl>);

impl From<Arc<dyn StackControl>> for Stacks {
    fn from(control: Arc<dyn StackControl>) -> Self {
        Self(control)
    }
}

impl Stacks {
    /// Every stack; see [`Orchestrator::status`].
    pub(super) async fn status(&self) -> Result<Vec<StackStatus>, OrchestratorError> {
        self.0.status().await
    }

    /// The [`UiEvent`]s sent from now on; see [`Orchestrator::subscribe`].
    pub(super) fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.0.subscribe()
    }

    /// One page of the event log; see [`StackControl::event_page`].
    pub(super) async fn event_page(
        &self,
        query: &EventQuery,
    ) -> Result<EventPage, OrchestratorError> {
        self.0.event_page(query).await
    }

    /// The stack called `name`.
    async fn find(&self, name: &str) -> Result<StackStatus, Outcome> {
        self.0
            .status()
            .await?
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| {
                Outcome::error(
                    StatusCode::NOT_FOUND,
                    format!("There is no stack called {name}."),
                )
            })
    }

    /// `GET /apply`: the form to paste or upload a stack file.
    pub async fn apply_form(shell: Shell) -> Page<ApplyPage> {
        Page(ApplyPage {
            layout: shell.layout(Section::Stacks),
        })
    }

    /// `POST /apply/preview`: the file's validation errors, or what applying it would do.
    pub async fn preview(
        Extension(stacks): Extension<Stacks>,
        StackFile(toml): StackFile,
    ) -> Response {
        match stacks.0.preview(&toml).await {
            Ok(preview) => Page(PreviewView::new(preview, toml)).into_response(),
            Err(err) => {
                ApplyFailed::new(Attempt::Check, Outcome::from(err).quoting(&toml)).into_response()
            }
        }
    }

    /// `POST /apply`: applies the confirmed file and redeploys what changes (ADR-0049), then
    /// sends the browser to the deployment, or says that nothing needed redeploying.
    pub async fn apply(
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
        StackFile(toml): StackFile,
    ) -> Response {
        match stacks.0.apply(&toml, &session.login().into()).await {
            Ok(outcome) => Self::applied(outcome),
            Err(err) => {
                ApplyFailed::new(Attempt::Apply, Outcome::from(err).quoting(&toml)).into_response()
            }
        }
    }

    /// `POST /stacks/{name}/reresolve/preview`: the digests the third-party tags of the active
    /// revision resolve to now, and what applying them would redeploy.
    pub async fn reresolve_preview(
        Extension(stacks): Extension<Stacks>,
        Path(name): Path<String>,
    ) -> Response {
        let revision = match stacks.find(&name).await {
            Ok(status) => status.revision.revision,
            Err(outcome) => return outcome.into_response(),
        };
        match stacks.0.reresolve_preview(&name).await {
            Ok(preview) => Page(ReresolveView::new(preview, revision)).into_response(),
            Err(err) => Outcome::from(err).into_response(),
        }
    }

    /// `POST /stacks/{name}/reresolve`: applies freshly resolved digests as a new revision and
    /// redeploys the services they move (ADR-0025, ADR-0049).
    pub async fn reresolve(
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
        Path(name): Path<String>,
        Form(form): Form<ReresolveForm>,
    ) -> Response {
        match stacks
            .0
            .reresolve(&name, form.revision, &session.login().into())
            .await
        {
            Ok(Some(outcome)) => Self::applied(outcome),
            Ok(None) => Outcome::success(format!(
                "Every tag of {name} still resolves to the digest it runs; nothing changed."
            ))
            .informing()
            .into_response(),
            Err(err) => Outcome::from(err).into_response(),
        }
    }

    /// The answer to an apply: sends the browser to the deployment, or says that nothing
    /// needed redeploying.
    fn applied(outcome: ApplyOutcome) -> Response {
        let stack = outcome.revision.stack.clone();
        let revision = outcome.revision.revision;
        match outcome.deployment {
            Some(deployment) => {
                let url = DeploymentView::url(&stack, deployment.deployment);
                let message = format!(
                    "Applied revision {revision} of {stack}; deployment {} is {}.",
                    deployment.deployment, deployment.state
                );
                let mut response = Outcome::success(message)
                    .with_link(url.clone(), "Follow the deployment")
                    .into_response();
                if let Ok(value) = url.parse() {
                    response.headers_mut().insert("hx-redirect", value);
                }
                response
            }
            None => Outcome::success(format!(
                "Applied revision {revision} of {stack}. Nothing to redeploy: every service \
                 already runs this configuration."
            ))
            .with_link(StackView::url(&stack), format!("Back to {stack}"))
            .into_response(),
        }
    }

    /// `POST /stacks/{name}/deploy`: deploys the digests typed into the form, one field per
    /// CI-built service (`digest.<service>`); empty fields keep the running digest.
    pub async fn deploy(
        Extension(stacks): Extension<Stacks>,
        Path(name): Path<String>,
        Form(fields): Form<Vec<(String, String)>>,
    ) -> Response {
        let images: DigestSet = fields
            .into_iter()
            .filter_map(|(key, value)| {
                let service = key.strip_prefix(DeployForm::FIELD_PREFIX)?;
                let digest = value.trim();
                (!digest.is_empty()).then(|| (service.to_owned(), digest.to_owned()))
            })
            .collect();
        if images.is_empty() {
            return Outcome::error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Enter a digest for at least one service.",
            )
            .into_response();
        }
        match stacks.0.deploy(&name, images).await {
            Ok(outcome) => DeployForm::outcome(&name, &outcome).into_response(),
            Err(err) => Outcome::from(err).into_response(),
        }
    }

    /// `POST /stacks/{name}/remove`: removes the stack once its name is typed as confirmation,
    /// keeping its volumes unless asked to delete them (ADR-0030).
    pub async fn remove(
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
        Path(name): Path<String>,
        Form(form): Form<RemoveForm>,
    ) -> Response {
        let confirmation = form.confirm.trim();
        if confirmation != name {
            return Outcome::error(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Type the stack's name, {name}, to confirm the removal."),
            )
            .into_response();
        }
        let request = if form.delete_volumes.is_some() {
            RemoveStack::deleting_volumes(&name, confirmation)
        } else {
            RemoveStack::keeping_volumes(&name)
        };
        match stacks.0.remove(request, &session.login().into()).await {
            Ok(removed) => {
                let volumes = if removed.volumes.is_empty() {
                    if form.delete_volumes.is_some() {
                        "It had no volumes to delete.".to_owned()
                    } else {
                        "Its volumes were kept.".to_owned()
                    }
                } else {
                    format!("Deleted volumes: {}.", removed.volumes.join(", "))
                };
                Outcome::success(format!(
                    "Removed stack {name} and its {}. {volumes} Its revisions and deployment \
                     history are kept.",
                    Counted(removed.containers, "container"),
                ))
                .with_link("/stacks".into(), "Back to the stacks")
                .into_response()
            }
            Err(err) => Outcome::from(err).into_response(),
        }
    }
}

/// The stack file of an apply form: the uploaded file if one was chosen, else the pasted text.
/// Accepts `multipart/form-data` (the upload form) and `application/x-www-form-urlencoded` (the
/// confirm form). Line endings are normalized to `\n`: browsers send form text with `\r\n`.
pub struct StackFile(String);

impl StackFile {
    /// The largest apply form accepted, in bytes (8 MiB; ADR-0057 §2). A stack file may hold
    /// 1 MiB of config file content, which the urlencoded confirm form can triple; axum's
    /// default of 2 MB would refuse valid files. A larger body is answered with 413.
    pub const MAX_BODY: usize = 8 * 1024 * 1024;
    const PASTED: &str = "toml";
    const UPLOADED: &str = "file";

    fn choose(pasted: Option<String>, uploaded: Option<Vec<u8>>) -> Result<Self, Outcome> {
        let uploaded = uploaded
            .filter(|bytes| !bytes.iter().all(u8::is_ascii_whitespace))
            .map(|bytes| {
                String::from_utf8(bytes).map_err(|_| {
                    Outcome::error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "The uploaded file is not UTF-8 text.",
                    )
                })
            })
            .transpose()?;
        let pasted = pasted.filter(|text| !text.trim().is_empty());
        uploaded
            .or(pasted)
            .map(|text| Self(text.replace("\r\n", "\n")))
            .ok_or_else(|| {
                Outcome::error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Paste the stack file or choose one to upload.",
                )
            })
    }
}

impl<S: Send + Sync> FromRequest<S> for StackFile {
    type Rejection = Outcome;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        // The rejection's own status: 413 for a body over `MAX_BODY`, 400 otherwise.
        let unreadable = |status: StatusCode, err: &dyn fmt::Display| {
            Outcome::error(status, format!("Cannot read the form: {err}"))
        };
        let multipart = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("multipart/form-data"));
        if !multipart {
            let Form(form) = Form::<PastedFile>::from_request(request, state)
                .await
                .map_err(|err| unreadable(err.status(), &err))?;
            return Self::choose(form.toml, None);
        }
        let mut fields = Multipart::from_request(request, state)
            .await
            .map_err(|err| unreadable(err.status(), &err))?;
        let (mut pasted, mut uploaded) = (None, None);
        while let Some(field) = fields
            .next_field()
            .await
            .map_err(|err| unreadable(err.status(), &err))?
        {
            match field.name() {
                Some(Self::PASTED) => {
                    pasted = Some(
                        field
                            .text()
                            .await
                            .map_err(|err| unreadable(err.status(), &err))?,
                    );
                }
                Some(Self::UPLOADED) => {
                    uploaded = Some(
                        field
                            .bytes()
                            .await
                            .map_err(|err| unreadable(err.status(), &err))?
                            .into(),
                    );
                }
                _ => {}
            }
        }
        Self::choose(pasted, uploaded)
    }
}

/// The urlencoded form of [`StackFile`].
#[derive(Deserialize)]
struct PastedFile {
    toml: Option<String>,
}

/// The manual deploy form.
struct DeployForm;

impl DeployForm {
    /// Each CI-built service's digest field is named `digest.<service>`.
    const FIELD_PREFIX: &str = "digest.";

    /// What a deploy request led to.
    fn outcome(stack: &str, outcome: &DeployOutcome) -> Outcome {
        let id = outcome.deployment;
        let url = DeploymentView::url(stack, id);
        match outcome.state {
            DeploymentState::Noop => Outcome {
                kind: FlashKind::Info,
                ..Outcome::success(format!(
                    "Noop: these digests already run (deployment {id})."
                ))
            },
            DeploymentState::Queued => Outcome::success(format!(
                "Accepted: deployment {id} is queued behind the stack's active deployment."
            ))
            .with_link(url, "Follow the deployment"),
            state => Outcome::success(format!("Accepted: deployment {id} is {state}."))
                .with_link(url, "Follow the deployment"),
        }
    }
}

/// The confirm form of "re-resolve tags": the revision that was reviewed.
#[derive(Deserialize)]
pub struct ReresolveForm {
    revision: u32,
}

/// The remove form.
#[derive(Deserialize)]
pub struct RemoveForm {
    /// The stack's name, typed again.
    #[serde(default)]
    confirm: String,
    /// Present when the volumes are to be deleted too.
    delete_volumes: Option<String>,
}

/// The answer to a form: a message, the problems found and where to go next, rendered as a
/// partial for the form's result area.
#[derive(Debug, Template)]
#[template(path = "stack_outcome.html")]
pub struct Outcome {
    status: StatusCode,
    kind: FlashKind,
    message: String,
    problems: Vec<Problem>,
    link: Option<Link>,
}

/// A link offered with an [`Outcome`].
#[derive(Debug)]
struct Link {
    href: String,
    label: String,
}

impl Outcome {
    pub(super) fn success(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            kind: FlashKind::Success,
            message: message.into(),
            problems: Vec::new(),
            link: None,
        }
    }

    pub(super) fn error(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            kind: FlashKind::Error,
            ..Self::success(message)
        }
    }

    /// The status the outcome is answered with.
    pub(super) fn status(&self) -> StatusCode {
        self.status
    }

    pub(super) fn with_link(mut self, href: String, label: impl Into<String>) -> Self {
        self.link = Some(Link {
            href,
            label: label.into(),
        });
        self
    }

    /// Shows each problem's line of `source` next to it.
    fn quoting(mut self, source: &str) -> Self {
        let lines: Vec<&str> = source.lines().collect();
        for problem in &mut self.problems {
            problem.quote(&lines);
        }
        self
    }

    /// Shown as information rather than as a success.
    pub(super) fn informing(mut self) -> Self {
        self.kind = FlashKind::Info;
        self
    }

    /// The outcome as a full page, for the read-only routes.
    pub(super) fn page(self, layout: Layout) -> Response {
        let status = self.status;
        (
            status,
            Page(ErrorPage {
                layout: layout.with_flash(self.into()),
            }),
        )
            .into_response()
    }
}

impl From<Outcome> for super::pages::Flash {
    fn from(outcome: Outcome) -> Self {
        Self {
            kind: outcome.kind,
            message: outcome.message,
        }
    }
}

impl From<OrchestratorError> for Outcome {
    fn from(err: OrchestratorError) -> Self {
        use OrchestratorError as E;
        let status = match &err {
            E::UnknownStack(_) | E::UnknownService(_) | E::UnknownDeployment(_) => {
                StatusCode::NOT_FOUND
            }
            E::Secret(SecretError::InvalidName { .. }) => StatusCode::UNPROCESSABLE_ENTITY,
            E::Forbidden { .. } => StatusCode::FORBIDDEN,
            E::ForeignDigest { .. }
            | E::Invalid(_)
            | E::Collision(_)
            | E::Resolve { .. }
            | E::NotCiBuilt(_)
            | E::Digest { .. }
            | E::NotRedeployable { .. }
            | E::VolumeConfirmation { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            E::StackBusy(_)
            | E::Removing(_)
            | E::RevisionChanged { .. }
            | E::NotCancellable { .. } => StatusCode::CONFLICT,
            E::Runtime(_) => StatusCode::BAD_GATEWAY,
            E::Secret(_) | E::Store(_) | E::Ports(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        if status.is_server_error() {
            tracing::error!(error = %err, "an orchestrator call failed");
        }
        let (message, problems) = match &err {
            E::Invalid(invalid) => (
                "The stack file is invalid.".to_owned(),
                Problem::all(invalid),
            ),
            E::Collision(collisions) => (
                "The stack's routes collide with routes hoster or another stack owns.".to_owned(),
                collisions
                    .collisions
                    .iter()
                    .map(|c| Problem::from(c.to_string()))
                    .collect(),
            ),
            other => (Self::sentence(&other.to_string()), Vec::new()),
        };
        Self {
            status,
            problems,
            ..Self::error(status, message)
        }
    }
}

impl Outcome {
    /// An error's message as a sentence.
    pub(super) fn sentence(text: &str) -> String {
        let mut chars = text.chars();
        match chars.next() {
            Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
            None => text.to_owned(),
        }
    }
}

impl IntoResponse for Outcome {
    fn into_response(self) -> Response {
        (self.status, Page(self)).into_response()
    }
}

/// One problem in a stack file, with its line when known.
#[derive(Debug)]
struct Problem {
    line: Option<usize>,
    column: Option<usize>,
    path: String,
    message: String,
    /// The line of the file it is on, filled in by [`Outcome::quoting`].
    source: Option<String>,
}

impl Problem {
    fn all(err: &StackError) -> Vec<Self> {
        err.diagnostics().iter().map(Self::from).collect()
    }

    /// Fills in [`Self::source`] from the file's `lines`.
    fn quote(&mut self, lines: &[&str]) {
        self.source = self
            .line
            .and_then(|line| lines.get(line.checked_sub(1)?))
            .map(|line| (*line).to_owned());
    }
}

impl From<&Diagnostic> for Problem {
    fn from(diagnostic: &Diagnostic) -> Self {
        Self {
            line: diagnostic.location.map(|l| l.line),
            column: diagnostic.location.map(|l| l.column),
            path: diagnostic.path.to_string(),
            message: diagnostic.message.clone(),
            source: None,
        }
    }
}

impl From<String> for Problem {
    fn from(message: String) -> Self {
        Self {
            line: None,
            column: None,
            path: String::new(),
            message,
            source: None,
        }
    }
}

/// A store timestamp as an RFC 3339 UTC time, to the second.
pub(super) struct When(pub(super) Timestamp);

impl fmt::Display for When {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let time = SystemTime::from(self.0);
        write!(f, "{}", humantime::format_rfc3339_seconds(time))
    }
}

/// A read-only page that could not be shown; the reason is in the flash.
#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPage {
    layout: Layout,
}

mod apply;
mod deployments;
mod environment;
mod frame;
mod list;
mod logs;
mod overview;

use apply::{ApplyFailed, ApplyPage, Attempt, PreviewView, ReresolveView};
pub use deployments::DeploymentView;
pub(super) use list::{Ago, Condition, LastDeploy, TrafficLine};
pub(super) use overview::StackView;

#[cfg(test)]
pub(super) mod tests;
