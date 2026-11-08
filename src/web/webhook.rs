//! The deploy webhook (ADR-0042, ADR-0047): `POST /hooks/deploy` with
//! `Authorization: Bearer <GitHub Actions OIDC token>` and a JSON body
//! `{"stack":"notes","images":{"api":"sha256:…"},"tag":"v1.4.2"}`.
//!
//! The token is verified first (see `oidc`), and its `jti` claimed in the state store, before
//! a byte of the body is read; the body is then read up to 64 KiB and parsed. Authorization
//! against the stack's `[deploy]` rule, the payload checks against the stack, the digest
//! ownership check and the deployment itself belong to the orchestrator
//! ([`Orchestrator::deploy_webhook`](crate::orchestrator::Orchestrator::deploy_webhook)).
//!
//! Answers: `200 {"result":"noop",…}`, `202 {"result":"accepted","deployment_id":…}`, `401`
//! for a bad token, `403` when the stack may not be deployed by this caller (an unknown stack
//! looks the same), `413`/`400`/`422` for payload problems. Bodies never say more than the
//! caller needs: the reasons behind a `401` or `403` go to the log and, for a valid token, to
//! the stack page's list of refused attempts, with the token's claims but never the token.

mod oidc;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Extension;
use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

pub use self::oidc::OidcVerifier;
use self::oidc::{Claims, TokenError};
use super::stacks::StackControl;
use crate::orchestrator::{DeployOutcome, OrchestratorError, WebhookDeploy};
use crate::stack::{DeployCaller, Digest, Name};
use crate::store::{DeploymentState, Store};

/// The webhook handler's collaborators.
#[derive(Clone)]
pub struct Webhook(Arc<Inner>);

struct Inner {
    verifier: OidcVerifier,
    /// Where used token ids are kept.
    store: Store,
    stacks: Arc<dyn StackControl>,
}

impl Webhook {
    /// The largest body accepted.
    pub const MAX_BODY: usize = 64 * 1024;
    /// The longest informational `tag` accepted. The tag is free text: the spec's CI snippet
    /// sends `github.ref_name`, which for a branch such as `feature/x` is no OCI tag.
    const MAX_TAG: usize = 256;

    pub fn new(verifier: OidcVerifier, store: Store, stacks: Arc<dyn StackControl>) -> Self {
        Self(Arc::new(Inner {
            verifier,
            store,
            stacks,
        }))
    }

    /// `POST /hooks/deploy`.
    pub async fn deploy(Extension(hook): Extension<Webhook>, request: Request) -> Response {
        let (parts, body) = request.into_parts();
        let claims = match hook.authenticate(&parts.headers).await {
            Ok(claims) => claims,
            Err(answer) => return answer.into_response(),
        };
        let payload = match Payload::read(body).await {
            Ok(payload) => payload,
            Err(refusal) => {
                hook.refused(&claims, refusal.stack.as_deref(), &refusal.reason)
                    .await;
                return refusal.answer.into_response();
            }
        };
        let stack = payload.stack.clone();
        let request = WebhookDeploy {
            stack: payload.stack,
            images: payload.images,
            caller: DeployCaller::from(&claims),
            provenance: claims.provenance(payload.tag),
        };
        match hook.0.stacks.deploy_webhook(request).await {
            Ok(outcome) => {
                tracing::info!(
                    stack, deployment = %outcome.deployment, state = %outcome.state,
                    repository = claims.repository, git_ref = claims.git_ref,
                    run_id = claims.run_id, "webhook deploy accepted"
                );
                Answer::from(&outcome).into_response()
            }
            Err(err) => {
                let answer = Answer::from(&err);
                if answer.status.is_client_error() && answer.status != StatusCode::CONFLICT {
                    hook.refused(&claims, Some(&stack), &err.to_string()).await;
                } else {
                    tracing::error!(stack, error = %err, "webhook deploy failed");
                }
                answer.into_response()
            }
        }
    }

    /// Verifies the bearer token and uses up its `jti`.
    async fn authenticate(&self, headers: &HeaderMap) -> Result<Claims, Answer> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let (scheme, token) = value.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
            })
            .filter(|token| !token.is_empty());
        let Some(token) = token else {
            tracing::warn!("webhook request without a bearer token refused");
            return Err(Answer::unauthorized());
        };
        let claims = match self.0.verifier.verify(token).await {
            Ok(claims) => claims,
            Err(TokenError::Unavailable(reason)) => {
                tracing::error!(reason, "webhook token cannot be verified");
                return Err(Answer::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "cannot verify tokens right now",
                ));
            }
            Err(err) => {
                tracing::warn!(error = %err, "webhook token refused");
                return Err(Answer::unauthorized());
            }
        };
        match self
            .0
            .store
            .claim_token(&claims.jti, claims.usable_until)
            .await
        {
            Ok(true) => Ok(claims),
            Ok(false) => {
                tracing::warn!(
                    jti = claims.jti,
                    repository = claims.repository,
                    run_id = claims.run_id,
                    "webhook token replayed"
                );
                Err(Answer::unauthorized())
            }
            Err(err) => {
                tracing::error!(error = %err, "cannot record the webhook token id");
                Err(Answer::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error",
                ))
            }
        }
    }

    /// Logs a refused attempt that carried a valid token, and records it on its stack.
    async fn refused(&self, claims: &Claims, stack: Option<&str>, reason: &str) {
        tracing::warn!(
            stack,
            reason,
            repository = claims.repository,
            repository_id = claims.repository_id,
            git_ref = claims.git_ref,
            environment = claims.environment,
            workflow_ref = claims.workflow_ref,
            job_workflow_ref = claims.job_workflow_ref,
            actor = claims.actor,
            run_id = claims.run_id,
            "webhook deploy refused"
        );
        if let Some(stack) = stack
            && let Err(err) = self
                .0
                .stacks
                .record_rejection(&claims.rejection(stack, reason))
                .await
        {
            tracing::error!(stack, error = %err, "cannot record the refused webhook attempt");
        }
    }
}

/// A checked request body.
#[derive(Debug)]
struct Payload {
    stack: String,
    images: BTreeMap<String, String>,
    tag: Option<String>,
}

/// The body as sent. `stack_file` is reserved (ADR-0027) and refused before this is parsed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayload {
    stack: String,
    images: BTreeMap<String, String>,
    tag: Option<String>,
}

/// A body that was refused, with the stack it names if that much was readable.
struct Refusal {
    stack: Option<String>,
    reason: String,
    answer: Answer,
}

impl Refusal {
    fn new(stack: Option<&str>, status: StatusCode, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            stack: stack.map(str::to_owned),
            reason: message.clone(),
            answer: Answer::error(status, message),
        }
    }
}

impl Payload {
    /// Reads at most [`Webhook::MAX_BODY`] bytes of `body` and checks them.
    async fn read(body: Body) -> Result<Self, Refusal> {
        let bytes = to_bytes(body, Webhook::MAX_BODY).await.map_err(|_| {
            Refusal::new(
                None,
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the body is larger than {} bytes", Webhook::MAX_BODY),
            )
        })?;
        Self::parse(&bytes)
    }

    fn parse(bytes: &[u8]) -> Result<Self, Refusal> {
        let bad_request = |stack: Option<&str>, message: String| {
            Refusal::new(stack, StatusCode::BAD_REQUEST, message)
        };
        let object: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(bytes)
            .map_err(|err| bad_request(None, format!("the body is not a JSON object: {err}")))?;
        let stack = object
            .get("stack")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let stack = stack.as_deref();
        let invalid =
            |message: String| Refusal::new(stack, StatusCode::UNPROCESSABLE_ENTITY, message);
        if object.contains_key("stack_file") {
            return Err(invalid(
                "`stack_file` is reserved: stack files are applied in the web UI".into(),
            ));
        }
        let raw: RawPayload = serde_json::from_value(serde_json::Value::Object(object))
            .map_err(|err| bad_request(stack, format!("invalid body: {err}")))?;
        raw.stack
            .parse::<Name>()
            .map_err(|err| invalid(format!("invalid stack: {err}")))?;
        if raw.images.is_empty() {
            return Err(invalid("`images` names no service".into()));
        }
        for (service, digest) in &raw.images {
            digest
                .parse::<Digest>()
                .map_err(|err| invalid(format!("invalid digest for service {service}: {err}")))?;
        }
        if let Some(tag) = &raw.tag
            && (tag.is_empty() || tag.len() > Webhook::MAX_TAG || tag.chars().any(char::is_control))
        {
            return Err(invalid("invalid `tag`".into()));
        }
        Ok(Self {
            stack: raw.stack,
            images: raw.images,
            tag: raw.tag,
        })
    }
}

/// A JSON answer.
#[derive(Debug)]
struct Answer {
    status: StatusCode,
    body: serde_json::Value,
}

impl Answer {
    fn error(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({ "error": message.into() }),
        }
    }

    fn unauthorized() -> Self {
        Self::error(StatusCode::UNAUTHORIZED, "invalid token")
    }
}

impl From<&DeployOutcome> for Answer {
    fn from(outcome: &DeployOutcome) -> Self {
        let id = outcome.deployment.to_string();
        match outcome.state {
            DeploymentState::Noop => Self {
                status: StatusCode::OK,
                body: json!({ "result": "noop", "deployment_id": id }),
            },
            state => Self {
                status: StatusCode::ACCEPTED,
                body: json!({
                    "result": "accepted",
                    "deployment_id": id,
                    "state": state.to_string(),
                }),
            },
        }
    }
}

/// What the caller learns of an orchestrator refusal. Only an authorized caller gets past
/// `Forbidden`, so the payload problems after it may name services.
impl From<&OrchestratorError> for Answer {
    fn from(err: &OrchestratorError) -> Self {
        use OrchestratorError as E;
        let unprocessable =
            |message: String| Self::error(StatusCode::UNPROCESSABLE_ENTITY, message);
        match err {
            E::Forbidden { .. } | E::UnknownStack(_) => {
                Self::error(StatusCode::FORBIDDEN, "not allowed to deploy this stack")
            }
            E::UnknownService(service) => unprocessable(format!("unknown service {service}")),
            E::NotCiBuilt(service) => {
                unprocessable(format!("service {service} does not use a CI-built image"))
            }
            E::Digest { service, .. } => {
                unprocessable(format!("invalid digest for service {service}"))
            }
            E::ForeignDigest { service, .. } => unprocessable(format!(
                "the digest for service {service} is not in its repository"
            )),
            E::Resolve { service, .. } => Self::error(
                StatusCode::BAD_GATEWAY,
                format!("cannot check the digest for service {service} with its registry"),
            ),
            E::StackBusy(_) | E::Removing(_) => Self::error(
                StatusCode::CONFLICT,
                "the stack cannot be deployed right now",
            ),
            _ => Self::error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
        }
    }
}

impl IntoResponse for Answer {
    fn into_response(self) -> Response {
        let mut response = (self.status, axum::Json(self.body)).into_response();
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        if self.status == StatusCode::UNAUTHORIZED {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

#[cfg(test)]
mod tests;
