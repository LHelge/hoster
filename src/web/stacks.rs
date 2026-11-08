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
//! sends the browser to the resulting deployment's page, which follows it until it ends.
//!
//! "Re-resolve tags" (ADR-0025) works the same way for the active revision: the preview shows
//! the digests every third-party tag resolves to now and the services that would move, and the
//! confirmation applies them as a new revision and redeploys (ADR-0049). The stack's secrets
//! (ADR-0029) have their own page; see `secrets.rs`. The deployment history, live progress and
//! "redeploy this one" are in `deployments.rs`.

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use askama::Template;
use axum::Extension;
use axum::extract::{Form, FromRequest, Multipart, Path, Request};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use similar::{ChangeTag, TextDiff};
use tokio::sync::broadcast;

use super::auth::Session;
use super::pages::{FlashKind, Layout, Page, Section};
use crate::orchestrator::{
    ApplyOutcome, ApplyPreview, DeployOutcome, Orchestrator, OrchestratorError, PinChange,
    RemoveOutcome, RemoveStack, SecretChange, ServiceState, StackSecret, StackStatus, UiEvent,
    WebhookDeploy,
};
use crate::runtime::ContainerRuntime;
use crate::secrets::{SecretError, SecretValue};
use crate::stack::{Diagnostic, StackError};
use crate::stats::Elapsed;
use crate::store::{
    Deployment, DeploymentEvent, DeploymentId, DeploymentSource, DeploymentState, DigestSet,
    Timestamp, WebhookRejection,
};

/// What the stack pages need from the orchestrator. Dyn-compatible, so the app holds the real
/// orchestrator (any runtime) or a fake in tests behind one `Arc`.
#[async_trait::async_trait]
pub trait StackControl: Send + Sync + 'static {
    /// Every stack; see [`Orchestrator::status`].
    async fn status(&self) -> Result<Vec<StackStatus>, OrchestratorError>;

    /// What applying `toml` would do; see [`Orchestrator::preview`].
    async fn preview(&self, toml: &str) -> Result<ApplyPreview, OrchestratorError>;

    /// Applies `toml` and redeploys what changes; see [`Orchestrator::apply`].
    async fn apply(&self, toml: &str) -> Result<ApplyOutcome, OrchestratorError>;

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

    /// See [`Orchestrator::remove`].
    async fn remove(&self, request: RemoveStack) -> Result<RemoveOutcome, OrchestratorError>;

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

    /// See [`Orchestrator::rollback`].
    async fn rollback(
        &self,
        stack: &str,
        id: DeploymentId,
    ) -> Result<DeployOutcome, OrchestratorError>;

    /// See [`Orchestrator::subscribe`].
    fn subscribe(&self) -> broadcast::Receiver<UiEvent>;

    /// See [`Orchestrator::reresolve_preview`].
    async fn reresolve_preview(&self, stack: &str) -> Result<ApplyPreview, OrchestratorError>;

    /// See [`Orchestrator::reresolve`].
    async fn reresolve(
        &self,
        stack: &str,
        expected: u32,
    ) -> Result<Option<ApplyOutcome>, OrchestratorError>;

    /// See [`Orchestrator::secrets`].
    async fn secrets(&self, stack: &str) -> Result<Vec<StackSecret>, OrchestratorError>;

    /// See [`Orchestrator::set_secret`].
    async fn set_secret(
        &self,
        stack: &str,
        name: &str,
        value: &SecretValue,
    ) -> Result<SecretChange, OrchestratorError>;

    /// See [`Orchestrator::delete_secret`].
    async fn delete_secret(
        &self,
        stack: &str,
        name: &str,
    ) -> Result<SecretChange, OrchestratorError>;
}

#[async_trait::async_trait]
impl<R: ContainerRuntime> StackControl for Arc<Orchestrator<R>> {
    async fn status(&self) -> Result<Vec<StackStatus>, OrchestratorError> {
        Orchestrator::status(self).await
    }

    async fn preview(&self, toml: &str) -> Result<ApplyPreview, OrchestratorError> {
        Orchestrator::preview(self, toml).await
    }

    async fn apply(&self, toml: &str) -> Result<ApplyOutcome, OrchestratorError> {
        Orchestrator::apply(self, toml).await
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

    async fn remove(&self, request: RemoveStack) -> Result<RemoveOutcome, OrchestratorError> {
        Orchestrator::remove(self, request).await
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

    async fn reresolve_preview(&self, stack: &str) -> Result<ApplyPreview, OrchestratorError> {
        Orchestrator::reresolve_preview(self, stack).await
    }

    async fn reresolve(
        &self,
        stack: &str,
        expected: u32,
    ) -> Result<Option<ApplyOutcome>, OrchestratorError> {
        Orchestrator::reresolve(self, stack, expected).await
    }

    async fn secrets(&self, stack: &str) -> Result<Vec<StackSecret>, OrchestratorError> {
        Orchestrator::secrets(self, stack).await
    }

    async fn set_secret(
        &self,
        stack: &str,
        name: &str,
        value: &SecretValue,
    ) -> Result<SecretChange, OrchestratorError> {
        Orchestrator::set_secret(self, stack, name, value).await
    }

    async fn delete_secret(
        &self,
        stack: &str,
        name: &str,
    ) -> Result<SecretChange, OrchestratorError> {
        Orchestrator::delete_secret(self, stack, name).await
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

    /// `GET /stacks`: every stack.
    pub async fn list(
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
    ) -> Response {
        let layout = Layout::new(Section::Stacks, &session);
        match stacks.0.status().await {
            Ok(list) => Page(StackList {
                layout,
                stacks: list.iter().map(StackView::from).collect(),
            })
            .into_response(),
            Err(err) => {
                let outcome = Outcome::from(err);
                let status = outcome.status;
                let page = Page(StackList {
                    layout: layout.with_flash(outcome.into()),
                    stacks: Vec::new(),
                });
                (status, page).into_response()
            }
        }
    }

    /// `GET /stacks/{name}`: one stack, with the deploy and remove forms.
    pub async fn show(
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
        Path(name): Path<String>,
    ) -> Response {
        let layout = Layout::new(Section::Stacks, &session);
        match stacks.find(&name).await {
            Ok(status) => Page(StackPage {
                layout,
                stack: StackView::from(&status),
            })
            .into_response(),
            Err(outcome) => outcome.page(layout),
        }
    }

    /// `GET /apply`: the form to paste or upload a stack file.
    pub async fn apply_form(Extension(session): Extension<Session>) -> Page<ApplyPage> {
        Page(ApplyPage {
            layout: Layout::new(Section::Stacks, &session),
        })
    }

    /// `POST /apply/preview`: the file's validation errors, or what applying it would do.
    pub async fn preview(
        Extension(stacks): Extension<Stacks>,
        StackFile(toml): StackFile,
    ) -> Response {
        match stacks.0.preview(&toml).await {
            Ok(preview) => Page(PreviewView::new(preview, toml)).into_response(),
            Err(err) => Outcome::from(err).quoting(&toml).into_response(),
        }
    }

    /// `POST /apply`: applies the confirmed file and redeploys what changes (ADR-0049), then
    /// sends the browser to the deployment, or says that nothing needed redeploying.
    pub async fn apply(
        Extension(stacks): Extension<Stacks>,
        StackFile(toml): StackFile,
    ) -> Response {
        match stacks.0.apply(&toml).await {
            Ok(outcome) => Self::applied(outcome),
            Err(err) => Outcome::from(err).quoting(&toml).into_response(),
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
        Path(name): Path<String>,
        Form(form): Form<ReresolveForm>,
    ) -> Response {
        match stacks.0.reresolve(&name, form.revision).await {
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
        match stacks.0.remove(request).await {
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
                    "Removed stack {name} and its {} container{}. {volumes} Its revisions and \
                     deployment history are kept.",
                    removed.containers,
                    if removed.containers == 1 { "" } else { "s" },
                ))
                .with_link("/stacks".into(), "Back to the stacks")
                .into_response()
            }
            Err(err) => Outcome::from(err).into_response(),
        }
    }

    /// `GET /stacks/{name}/deployments/{id}`: one deployment, followed until it ends.
    pub async fn deployment_page(
        Extension(stacks): Extension<Stacks>,
        Extension(session): Extension<Session>,
        Path((name, id)): Path<(String, String)>,
    ) -> Response {
        let layout = Layout::new(Section::Stacks, &session);
        match stacks.lookup(&name, &id).await {
            Ok(deployment) => Page(DeploymentPage { layout, deployment }).into_response(),
            Err(outcome) => outcome.page(layout),
        }
    }

    /// `GET /stacks/{name}/deployments/{id}/status`: the deployment's status partial, the
    /// one the page's live stream sends (see `deployments.rs`).
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

    pub(super) async fn lookup(&self, stack: &str, id: &str) -> Result<DeploymentView, Outcome> {
        let id = DeploymentView::parse_id(id)?;
        let (deployment, events) = self.0.deployment(stack, id).await?;
        Ok(DeploymentView::new(&deployment, &events))
    }
}

/// The stack file of an apply form: the uploaded file if one was chosen, else the pasted text.
/// Accepts `multipart/form-data` (the upload form) and `application/x-www-form-urlencoded` (the
/// confirm form). Line endings are normalized to `\n`: browsers send form text with `\r\n`.
pub struct StackFile(String);

impl StackFile {
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
        let unreadable = |err: &dyn fmt::Display| {
            Outcome::error(
                StatusCode::BAD_REQUEST,
                format!("Cannot read the form: {err}"),
            )
        };
        let multipart = request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("multipart/form-data"));
        if !multipart {
            let Form(form) = Form::<PastedFile>::from_request(request, state)
                .await
                .map_err(|err| unreadable(&err))?;
            return Self::choose(form.toml, None);
        }
        let mut fields = Multipart::from_request(request, state)
            .await
            .map_err(|err| unreadable(&err))?;
        let (mut pasted, mut uploaded) = (None, None);
        while let Some(field) = fields.next_field().await.map_err(|err| unreadable(&err))? {
            match field.name() {
                Some(Self::PASTED) => {
                    pasted = Some(field.text().await.map_err(|err| unreadable(&err))?);
                }
                Some(Self::UPLOADED) => {
                    uploaded = Some(field.bytes().await.map_err(|err| unreadable(&err))?.into());
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

/// A digest shortened for tables: `sha256:` and the first 12 hex digits.
pub(super) struct ShortDigest<'a>(pub(super) &'a str);

impl fmt::Display for ShortDigest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (algorithm, hex) = self.0.split_once(':').unwrap_or(("", self.0));
        let short = hex.get(..12).unwrap_or(hex);
        if algorithm.is_empty() {
            f.write_str(short)
        } else {
            write!(f, "{algorithm}:{short}")
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

/// A stack as the pages show it.
#[derive(Debug)]
pub struct StackView {
    name: String,
    url: String,
    revision: u32,
    /// The active revision's content hash, shortened, and in full.
    hash: String,
    full_hash: String,
    applied_at: String,
    services: Vec<ServiceView>,
    /// Whether any service takes its digest from deploys, so the deploy form has fields.
    deployable: bool,
    /// Whether any service uses a third-party image, so "re-resolve tags" has work to do.
    third_party: bool,
    last: Option<DeploymentSummary>,
    /// The deployment running now, as (id, state, url).
    active: Option<(String, String, String)>,
    queued: Option<String>,
    flag: Option<String>,
    /// Refused webhook attempts, newest first (ADR-0047).
    rejections: Vec<RejectionView>,
}

impl StackView {
    pub(super) fn url(stack: &str) -> String {
        format!("/stacks/{stack}")
    }

    pub fn secrets_url(&self) -> String {
        format!("{}/secrets", self.url)
    }

    pub fn reresolve_url(&self) -> String {
        format!("{}/reresolve/preview", self.url)
    }

    pub fn deploy_url(&self) -> String {
        format!("{}/deploy", self.url)
    }

    pub fn remove_url(&self) -> String {
        format!("{}/remove", self.url)
    }

    pub fn history_url(&self) -> String {
        format!("{}/deployments", self.url)
    }
}

impl From<&StackStatus> for StackView {
    fn from(status: &StackStatus) -> Self {
        let hash = status.revision.hash.to_string();
        let services: Vec<ServiceView> = status.services.iter().map(ServiceView::from).collect();
        Self {
            url: Self::url(&status.name),
            name: status.name.clone(),
            revision: status.revision.revision,
            hash: hash.get(..12).unwrap_or(&hash).to_owned(),
            full_hash: hash.clone(),
            applied_at: When(status.revision.created_at).to_string(),
            deployable: services.iter().any(|s| s.ci_built),
            third_party: services.iter().any(|s| !s.ci_built),
            services,
            last: status.last.as_ref().map(DeploymentSummary::from),
            active: status.active.map(|(id, state)| {
                (
                    id.to_string(),
                    state.to_string(),
                    DeploymentView::url(&status.name, id),
                )
            }),
            queued: status.queued.map(|id| id.to_string()),
            flag: status.flag.clone(),
            rejections: status.rejections.iter().map(RejectionView::from).collect(),
        }
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

/// A service as the stack pages show it.
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

impl From<&crate::orchestrator::ServiceStatus> for ServiceView {
    fn from(service: &crate::orchestrator::ServiceStatus) -> Self {
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

/// A deployment in one line.
#[derive(Debug)]
struct DeploymentSummary {
    id: String,
    url: String,
    state: String,
    source: String,
    at: String,
}

impl From<&Deployment> for DeploymentSummary {
    fn from(deployment: &Deployment) -> Self {
        Self {
            id: deployment.id.to_string(),
            url: DeploymentView::url(&deployment.stack, deployment.id),
            state: deployment.state.to_string(),
            source: deployment.source.to_string(),
            at: When(deployment.updated_at).to_string(),
        }
    }
}

/// A deployment with its transitions, as its page shows it.
#[derive(Debug)]
pub struct DeploymentView {
    id: String,
    stack: String,
    stack_url: String,
    url: String,
    state: String,
    source: String,
    generation: Option<u32>,
    error: Option<String>,
    target: Vec<(String, String)>,
    /// Where a webhook deployment came from, as (label, value) rows.
    provenance: Vec<(&'static str, String)>,
    events: Vec<EventView>,
    created_at: String,
    finished_at: Option<String>,
    /// Whether it is over; a running one polls for its status.
    finished: bool,
    /// Whether it may still be cancelled: active and before `Switching` (ADR-0040).
    cancellable: bool,
    /// Whether it went live, so it can be redeployed as a rollback (ADR-0041).
    redeployable: bool,
    /// How long it took, or has been running.
    duration: String,
}

impl DeploymentView {
    pub(super) fn url(stack: &str, id: DeploymentId) -> String {
        format!("/stacks/{stack}/deployments/{id}")
    }

    fn parse_id(id: &str) -> Result<DeploymentId, Outcome> {
        id.parse()
            .map_err(|_| Outcome::error(StatusCode::NOT_FOUND, format!("No deployment {id}.")))
    }

    fn new(deployment: &Deployment, events: &[DeploymentEvent]) -> Self {
        Self {
            id: deployment.id.to_string(),
            stack: deployment.stack.clone(),
            stack_url: StackView::url(&deployment.stack),
            url: Self::url(&deployment.stack, deployment.id),
            state: deployment.state.to_string(),
            source: deployment.source.to_string(),
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
                        ("Repository", p.repository.clone()),
                        ("Ref", p.git_ref.clone()),
                        ("Commit", p.sha.clone()),
                        ("Run", p.run_id.clone()),
                        ("Actor", p.actor.clone()),
                    ];
                    if let Some(tag) = &p.tag {
                        rows.push(("Tag", tag.clone()));
                    }
                    rows
                })
                .unwrap_or_default(),
            events: events.iter().map(EventView::from).collect(),
            created_at: When(deployment.created_at).to_string(),
            finished_at: deployment.finished_at.map(|t| When(t).to_string()),
            finished: deployment.state.is_terminal(),
            cancellable: deployment.state.is_cancellable(),
            redeployable: deployment.state.is_redeployable(),
            duration: Elapsed(deployment.duration()).to_string(),
        }
    }

    /// The live progress stream (SSE) the page follows while the deployment runs.
    pub fn events_url(&self) -> String {
        format!("{}/events", self.url)
    }

    pub fn redeploy_url(&self) -> String {
        format!("{}/redeploy", self.url)
    }

    pub fn history_url(&self) -> String {
        format!("{}/deployments", self.stack_url)
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

/// What applying a file would do, with the file itself for the confirm form.
#[derive(Template)]
#[template(path = "apply_preview.html")]
pub struct PreviewView {
    stack: String,
    stack_url: String,
    /// The revision being replaced, as "revision N (hash)"; `None` for a new stack.
    replaces: Option<String>,
    warnings: Vec<Problem>,
    pins: Vec<PinView>,
    changed: Vec<String>,
    diff: FileDiff,
    toml: String,
}

impl PreviewView {
    fn new(preview: ApplyPreview, toml: String) -> Self {
        let current = preview.current.as_ref();
        let old = current.map_or("", |r| r.toml.as_str());
        let lines: Vec<&str> = toml.lines().collect();
        let warnings = preview
            .warnings
            .iter()
            .map(|w| {
                let mut problem = Problem::from(w);
                problem.quote(&lines);
                problem
            })
            .collect();
        Self {
            stack_url: StackView::url(&preview.stack),
            replaces: current.map(|r| {
                let hash = r.hash.to_string();
                format!(
                    "revision {} ({})",
                    r.revision,
                    hash.get(..12).unwrap_or(&hash)
                )
            }),
            warnings,
            pins: preview.pins.iter().map(PinView::from).collect(),
            changed: preview.changed,
            diff: FileDiff::between(old, &toml),
            stack: preview.stack,
            toml,
        }
    }
}

/// A third-party service's pinned digest, before and after.
#[derive(Debug)]
struct PinView {
    service: String,
    before: Option<String>,
    after: String,
    changes: bool,
}

impl From<&PinChange> for PinView {
    fn from(pin: &PinChange) -> Self {
        Self {
            service: pin.service.clone(),
            before: pin.before.clone(),
            after: pin.after.clone(),
            changes: pin.before.as_deref() != Some(pin.after.as_str()),
        }
    }
}

/// What "re-resolve tags" would do: the pins before and after, and what moves.
#[derive(Template)]
#[template(path = "reresolve_preview.html")]
pub struct ReresolveView {
    stack: String,
    confirm_url: String,
    /// The revision being re-resolved, which the confirmation names.
    revision: u32,
    pins: Vec<PinView>,
    changed: Vec<String>,
    /// Whether any digest differs from the active revision's.
    moves: bool,
}

impl ReresolveView {
    fn new(preview: ApplyPreview, revision: u32) -> Self {
        let pins: Vec<PinView> = preview.pins.iter().map(PinView::from).collect();
        Self {
            confirm_url: format!("{}/reresolve", StackView::url(&preview.stack)),
            moves: pins.iter().any(|p| p.changes),
            pins,
            changed: preview.changed,
            stack: preview.stack,
            revision,
        }
    }
}

/// A line diff between the active revision and the new file, in hunks with context.
#[derive(Debug, Default)]
struct FileDiff {
    hunks: Vec<Vec<DiffLine>>,
}

#[derive(Debug, PartialEq, Eq)]
struct DiffLine {
    /// `add`, `remove` or `same`: the CSS class suffix.
    kind: &'static str,
    /// `+`, `-` or a space.
    sign: char,
    old: Option<usize>,
    new: Option<usize>,
    text: String,
}

impl FileDiff {
    /// Unchanged lines shown around each change.
    const CONTEXT: usize = 3;

    fn between(old: &str, new: &str) -> Self {
        let diff = TextDiff::from_lines(old, new);
        let hunks = diff
            .grouped_ops(Self::CONTEXT)
            .iter()
            .map(|group| {
                group
                    .iter()
                    .flat_map(|op| diff.iter_changes(op))
                    .map(|change| {
                        let (kind, sign) = match change.tag() {
                            ChangeTag::Insert => ("add", '+'),
                            ChangeTag::Delete => ("remove", '-'),
                            ChangeTag::Equal => ("same", ' '),
                        };
                        DiffLine {
                            kind,
                            sign,
                            old: change.old_index().map(|i| i + 1),
                            new: change.new_index().map(|i| i + 1),
                            text: change.value().trim_end_matches(['\n', '\r']).to_owned(),
                        }
                    })
                    .collect()
            })
            .collect();
        Self { hunks }
    }

    fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }
}

/// `GET /stacks`.
#[derive(Template)]
#[template(path = "stacks.html")]
pub struct StackList {
    layout: Layout,
    stacks: Vec<StackView>,
}

/// `GET /stacks/{name}`.
#[derive(Template)]
#[template(path = "stack.html")]
pub struct StackPage {
    layout: Layout,
    stack: StackView,
}

/// `GET /apply`.
#[derive(Template)]
#[template(path = "apply.html")]
pub struct ApplyPage {
    layout: Layout,
}

/// `GET /stacks/{name}/deployments/{id}`.
#[derive(Template)]
#[template(path = "deployment.html")]
pub struct DeploymentPage {
    layout: Layout,
    deployment: DeploymentView,
}

/// `GET /stacks/{name}/deployments/{id}/status`.
#[derive(Template)]
#[template(path = "deployment_status.html")]
pub struct DeploymentStatus {
    deployment: DeploymentView,
}

/// A read-only page that could not be shown; the reason is in the flash.
#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPage {
    layout: Layout,
}

mod deployments;
mod secrets;

#[cfg(test)]
pub(super) mod tests;
