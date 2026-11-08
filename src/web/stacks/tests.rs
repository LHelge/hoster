//! The stack pages against a fake orchestrator, through the whole app (sessions, CSRF), and
//! their templates rendered on their own.

use std::sync::Mutex;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt as _;

use super::*;
use crate::health::HealthStats;
use crate::orchestrator::{RestartStats, ServiceState, ServiceStatus};
use crate::runtime::VolumeMount;
use crate::stack::Stack;
use crate::store::{ContentHash, RevisionId, StackRevision};
use crate::web::auth::tests::{Browser, ORIGIN, TestApp};

pub const NOTES: &str = r#"version = 1
name = "notes"

[services.api]
image = "ghcr.io/acme/notes-api"

[[services.api.routes]]
host = "notes.example.com"
port = 8080

[services.storage]
image = "docker.io/rustfs/rustfs:1.0.0"
"#;

pub(super) fn digest(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

pub(super) fn revision(stack: &str, number: u32, toml: &str) -> StackRevision {
    StackRevision {
        id: RevisionId::from(i64::from(number)),
        stack: stack.into(),
        revision: number,
        toml: toml.into(),
        hash: ContentHash::of(toml),
        created_at: Timestamp::now(),
    }
}

pub(super) fn deployment(id: i64, state: DeploymentState) -> Deployment {
    Deployment {
        id: DeploymentId::from(id),
        stack: "notes".into(),
        revision: Some(RevisionId::from(1)),
        generation: Some(2),
        state,
        source: DeploymentSource::Apply,
        target: DigestSet::from([("api".into(), digest(1))]),
        provenance: None,
        error: None,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        committed_at: None,
        finished_at: state.is_terminal().then(Timestamp::now),
    }
}

/// The `notes` stack, at revision 1 of [`NOTES`], api running generation 2.
pub(super) fn notes_status() -> StackStatus {
    StackStatus {
        name: "notes".into(),
        revision: revision("notes", 1, NOTES),
        services: vec![
            ServiceStatus {
                name: "api".into(),
                image: "ghcr.io/acme/notes-api".into(),
                ci_built: true,
                state: ServiceState::Running {
                    generation: 2,
                    digest: digest(1),
                },
                restarts: RestartStats::default(),
                health: HealthStats::default(),
            },
            ServiceStatus {
                name: "storage".into(),
                image: "docker.io/rustfs/rustfs:1.0.0".into(),
                ci_built: false,
                state: ServiceState::NotDeployed,
                restarts: RestartStats::default(),
                health: HealthStats::default(),
            },
        ],
        active: None,
        queued: None,
        last: Some(deployment(3, DeploymentState::Live)),
        flag: Some("revision 1: deploy failed: <pull> denied".into()),
        rejections: Vec::new(),
    }
}

/// A call the stack pages made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Preview(String),
    Apply(String),
    Deploy(String, DigestSet),
    Webhook(Box<WebhookDeploy>),
    Remove(RemoveStack),
    ReresolvePreview(String),
    Reresolve(String, u32),
    /// A stored secret, with the value it was given (only the fake keeps it, to prove the
    /// pages never show it).
    SetSecret(String, String, String),
    DeleteSecret(String, String),
    Cancel(String, DeploymentId),
    Rollback(String, DeploymentId),
}

/// The value the secret fakes hold; it must never appear in a page.
pub const SECRET_VALUE: &str = "s3cr3t-Value-42";

/// The secrets of `notes`: `db` set and used by api, `old` set and unused, `missing` used by
/// api but not set.
fn notes_secrets() -> Vec<StackSecret> {
    vec![
        StackSecret {
            name: "db".into(),
            updated_at: Some(Timestamp::now()),
            services: vec!["api".into()],
        },
        StackSecret {
            name: "missing".into(),
            updated_at: None,
            services: vec!["api".into()],
        },
        StackSecret {
            name: "old".into(),
            updated_at: Some(Timestamp::now()),
            services: Vec::new(),
        },
    ]
}

/// An orchestrator that knows the stacks it is given, validates stack files for real and
/// records every call that would change something.
pub struct FakeStacks {
    pub stacks: Mutex<Vec<StackStatus>>,
    pub deployments: Mutex<Vec<(Deployment, Vec<DeploymentEvent>)>>,
    /// The services a preview or apply reports as changing.
    pub changed: Mutex<Vec<String>>,
    /// The state a deploy answers with.
    pub deploy_state: Mutex<DeploymentState>,
    /// The secrets of `notes`.
    pub secrets: Mutex<Vec<StackSecret>>,
    /// Whether a re-resolve finds a moved digest.
    pub tags_moved: Mutex<bool>,
    pub calls: Mutex<Vec<Call>>,
    /// Digests a webhook deploy finds in some other repository than its service's.
    pub foreign: Mutex<Vec<String>>,
    /// Every refused webhook attempt recorded, oldest first.
    pub rejections: Mutex<Vec<WebhookRejection>>,
    /// The `ui_events` the deployment streams follow.
    pub events: broadcast::Sender<UiEvent>,
    /// Volumes only earlier revisions declared, by podman name (`hoster-<stack>-<volume>`).
    pub earlier_volumes: Mutex<Vec<String>>,
}

impl Default for FakeStacks {
    fn default() -> Self {
        Self {
            stacks: Mutex::new(vec![notes_status()]),
            deployments: Mutex::new(Vec::new()),
            changed: Mutex::new(vec!["api".into()]),
            deploy_state: Mutex::new(DeploymentState::Received),
            secrets: Mutex::new(notes_secrets()),
            tags_moved: Mutex::new(true),
            calls: Mutex::new(Vec::new()),
            foreign: Mutex::new(Vec::new()),
            rejections: Mutex::new(Vec::new()),
            events: broadcast::channel(16).0,
            earlier_volumes: Mutex::new(Vec::new()),
        }
    }
}

impl FakeStacks {
    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn current(&self, stack: &str) -> Option<StackStatus> {
        self.stacks
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.name == stack)
            .cloned()
    }
}

#[async_trait::async_trait]
impl StackControl for FakeStacks {
    async fn status(&self) -> Result<Vec<StackStatus>, OrchestratorError> {
        Ok(self.stacks.lock().unwrap().clone())
    }

    async fn preview(&self, toml: &str) -> Result<ApplyPreview, OrchestratorError> {
        self.record(Call::Preview(toml.into()));
        let parsed = Stack::parse(toml)?;
        let stack = parsed.stack.name.to_string();
        let current = self.current(&stack);
        Ok(ApplyPreview {
            pins: vec![PinChange {
                service: "storage".into(),
                before: current.as_ref().map(|_| digest(0xaa)),
                after: digest(0xbb),
            }],
            current: current.map(|s| s.revision),
            stack,
            warnings: parsed.warnings,
            changed: self.changed.lock().unwrap().clone(),
        })
    }

    async fn apply(&self, toml: &str) -> Result<ApplyOutcome, OrchestratorError> {
        self.record(Call::Apply(toml.into()));
        let stack = Stack::parse(toml)?.stack.name.to_string();
        let changed = self.changed.lock().unwrap().clone();
        Ok(ApplyOutcome {
            revision: revision(&stack, 2, toml),
            warnings: Vec::new(),
            deployment: (!changed.is_empty()).then(|| DeployOutcome {
                deployment: DeploymentId::from(7),
                state: DeploymentState::Validating,
            }),
            changed,
        })
    }

    async fn deploy(
        &self,
        stack: &str,
        images: DigestSet,
    ) -> Result<DeployOutcome, OrchestratorError> {
        self.record(Call::Deploy(stack.into(), images.clone()));
        let status = self
            .current(stack)
            .ok_or_else(|| OrchestratorError::UnknownStack(stack.into()))?;
        for service in images.keys() {
            match status.services.iter().find(|s| &s.name == service) {
                None => return Err(OrchestratorError::UnknownService(service.clone())),
                Some(s) if !s.ci_built => {
                    return Err(OrchestratorError::NotCiBuilt(service.clone()));
                }
                Some(_) => {}
            }
        }
        Ok(DeployOutcome {
            deployment: DeploymentId::from(8),
            state: *self.deploy_state.lock().unwrap(),
        })
    }

    /// Like the orchestrator: the stack's `[deploy]` rule first, then the services and digests.
    async fn deploy_webhook(
        &self,
        request: WebhookDeploy,
    ) -> Result<DeployOutcome, OrchestratorError> {
        self.record(Call::Webhook(Box::new(request.clone())));
        let forbidden = |reason: &str| OrchestratorError::Forbidden {
            stack: request.stack.clone(),
            reason: reason.into(),
        };
        let status = self
            .current(&request.stack)
            .ok_or_else(|| forbidden("the stack does not exist"))?;
        let definition: Stack = status.revision.toml.parse()?;
        definition
            .deploy
            .as_ref()
            .ok_or_else(|| forbidden("the stack file has no [deploy] section"))?
            .check(&request.caller)
            .map_err(|denied| forbidden(&denied.to_string()))?;
        let images = request.images.clone();
        for (service, digest) in &images {
            if self.foreign.lock().unwrap().contains(digest) {
                return Err(OrchestratorError::ForeignDigest {
                    service: service.clone(),
                    repository: "ghcr.io/acme/notes-api".into(),
                });
            }
        }
        let calls = self.calls.lock().unwrap().len();
        let outcome = self.deploy(&request.stack, images).await;
        // `deploy` recorded a manual deploy call too; this was one webhook call.
        self.calls.lock().unwrap().truncate(calls);
        outcome
    }

    async fn record_rejection(
        &self,
        rejection: &WebhookRejection,
    ) -> Result<(), OrchestratorError> {
        self.rejections.lock().unwrap().push(rejection.clone());
        if let Some(status) = self
            .stacks
            .lock()
            .unwrap()
            .iter_mut()
            .find(|s| s.name == rejection.stack)
        {
            status.rejections.insert(0, rejection.clone());
        }
        Ok(())
    }

    async fn remove(&self, request: RemoveStack) -> Result<RemoveOutcome, OrchestratorError> {
        self.record(Call::Remove(request.clone()));
        if self.current(&request.stack).is_none() {
            return Err(OrchestratorError::UnknownStack(request.stack));
        }
        Ok(RemoveOutcome {
            containers: 2,
            volumes: if request.delete_volumes {
                vec!["hoster-notes-data".into()]
            } else {
                Vec::new()
            },
        })
    }

    async fn deployment(
        &self,
        stack: &str,
        id: DeploymentId,
    ) -> Result<(Deployment, Vec<DeploymentEvent>), OrchestratorError> {
        self.deployments
            .lock()
            .unwrap()
            .iter()
            .find(|(d, _)| d.id == id && d.stack == stack)
            .cloned()
            .ok_or(OrchestratorError::UnknownDeployment(id))
    }

    async fn cancel(&self, stack: &str, id: DeploymentId) -> Result<(), OrchestratorError> {
        self.record(Call::Cancel(stack.into(), id));
        let (deployment, _) = self.deployment(stack, id).await?;
        if deployment.state.is_cancellable() {
            Ok(())
        } else {
            Err(OrchestratorError::NotCancellable {
                id,
                state: deployment.state,
            })
        }
    }

    async fn history(&self, stack: &str, limit: u32) -> Result<Vec<Deployment>, OrchestratorError> {
        self.current(stack)
            .ok_or_else(|| OrchestratorError::UnknownStack(stack.into()))?;
        let mut deployments: Vec<Deployment> = self
            .deployments
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, _)| d.stack == stack)
            .map(|(d, _)| d.clone())
            .collect();
        deployments.sort_by_key(|d| std::cmp::Reverse(d.id));
        deployments.truncate(limit as usize);
        Ok(deployments)
    }

    async fn latest_live(&self, stack: &str) -> Result<Option<Deployment>, OrchestratorError> {
        Ok(self
            .history(stack, u32::MAX)
            .await?
            .into_iter()
            .find(|d| d.state.is_redeployable()))
    }

    async fn volumes(&self, stack: &str) -> Result<BTreeSet<String>, OrchestratorError> {
        let status = self
            .current(stack)
            .ok_or_else(|| OrchestratorError::UnknownStack(stack.into()))?;
        let prefix = format!("hoster-{stack}-");
        let mut volumes: BTreeSet<String> = self
            .earlier_volumes
            .lock()
            .unwrap()
            .iter()
            .filter(|volume| volume.starts_with(&prefix))
            .cloned()
            .collect();
        if let Ok(definition) = status.revision.toml.parse::<Stack>() {
            volumes
                .extend(VolumeMount::of(&definition).map(|(_, mount)| mount.volume().to_owned()));
        }
        Ok(volumes)
    }

    async fn rollback(
        &self,
        stack: &str,
        id: DeploymentId,
    ) -> Result<DeployOutcome, OrchestratorError> {
        self.record(Call::Rollback(stack.into(), id));
        let (deployment, _) = self.deployment(stack, id).await?;
        if !deployment.state.is_redeployable() {
            return Err(OrchestratorError::NotRedeployable {
                id,
                reason: "it never went live".into(),
            });
        }
        Ok(DeployOutcome {
            deployment: DeploymentId::from(20),
            state: *self.deploy_state.lock().unwrap(),
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<UiEvent> {
        self.events.subscribe()
    }

    async fn reresolve_preview(&self, stack: &str) -> Result<ApplyPreview, OrchestratorError> {
        self.record(Call::ReresolvePreview(stack.into()));
        let status = self
            .current(stack)
            .ok_or_else(|| OrchestratorError::UnknownStack(stack.into()))?;
        let moved = *self.tags_moved.lock().unwrap();
        Ok(ApplyPreview {
            stack: stack.into(),
            current: Some(status.revision),
            warnings: Vec::new(),
            pins: vec![PinChange {
                service: "storage".into(),
                before: Some(digest(0xaa)),
                after: digest(if moved { 0xbb } else { 0xaa }),
            }],
            changed: if moved {
                vec!["storage".into()]
            } else {
                Vec::new()
            },
        })
    }

    async fn reresolve(
        &self,
        stack: &str,
        expected: u32,
    ) -> Result<Option<ApplyOutcome>, OrchestratorError> {
        self.record(Call::Reresolve(stack.into(), expected));
        let status = self
            .current(stack)
            .ok_or_else(|| OrchestratorError::UnknownStack(stack.into()))?;
        if status.revision.revision != expected {
            return Err(OrchestratorError::RevisionChanged {
                stack: stack.into(),
                expected,
                active: status.revision.revision,
            });
        }
        if !*self.tags_moved.lock().unwrap() {
            return Ok(None);
        }
        Ok(Some(ApplyOutcome {
            revision: revision(stack, expected + 1, &status.revision.toml),
            warnings: Vec::new(),
            changed: vec!["storage".into()],
            deployment: Some(DeployOutcome {
                deployment: DeploymentId::from(9),
                state: DeploymentState::Validating,
            }),
        }))
    }

    async fn secrets(&self, stack: &str) -> Result<Vec<StackSecret>, OrchestratorError> {
        self.current(stack)
            .ok_or_else(|| OrchestratorError::UnknownStack(stack.into()))?;
        Ok(self.secrets.lock().unwrap().clone())
    }

    async fn set_secret(
        &self,
        stack: &str,
        name: &str,
        value: &SecretValue,
    ) -> Result<SecretChange, OrchestratorError> {
        self.record(Call::SetSecret(
            stack.into(),
            name.into(),
            value.expose().into(),
        ));
        let current = self.secrets(stack).await?;
        let found = current.iter().find(|s| s.name == name);
        let services = found.map(|s| s.services.clone()).unwrap_or_default();
        Ok(SecretChange {
            secret: format!("{stack}/{name}"),
            existed: found.is_some_and(|s| s.updated_at.is_some()),
            deployment: (!services.is_empty()).then(|| DeployOutcome {
                deployment: DeploymentId::from(11),
                state: DeploymentState::Validating,
            }),
            services,
        })
    }

    async fn delete_secret(
        &self,
        stack: &str,
        name: &str,
    ) -> Result<SecretChange, OrchestratorError> {
        self.record(Call::DeleteSecret(stack.into(), name.into()));
        let current = self.secrets(stack).await?;
        let found = current
            .iter()
            .find(|s| s.name == name && s.updated_at.is_some());
        let services = found.map(|s| s.services.clone()).unwrap_or_default();
        Ok(SecretChange {
            secret: format!("{stack}/{name}"),
            existed: found.is_some(),
            deployment: (!services.is_empty()).then(|| DeployOutcome {
                deployment: DeploymentId::from(12),
                state: DeploymentState::Validating,
            }),
            services,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Through the app
// ---------------------------------------------------------------------------------------------

pub async fn body(response: Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// An HTMX form post from a signed-in browser.
pub fn post(
    uri: &str,
    browser: &Browser,
    content_type: &str,
    body: impl Into<Body>,
) -> Request<Body> {
    Request::post(uri)
        .header(header::COOKIE, &browser.cookie)
        .header(header::ORIGIN, ORIGIN)
        .header("x-csrf-token", &browser.csrf)
        .header("hx-request", "true")
        .header(header::CONTENT_TYPE, content_type)
        .body(body.into())
        .unwrap()
}

pub const FORM: &str = "application/x-www-form-urlencoded";

pub fn encoded(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| {
            let encode = |s: &str| {
                s.bytes()
                    .map(|b| match b {
                        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                            (b as char).to_string()
                        }
                        b' ' => "+".into(),
                        b => format!("%{b:02X}"),
                    })
                    .collect::<String>()
            };
            format!("{}={}", encode(k), encode(v))
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// A `multipart/form-data` body with the pasted text and an uploaded file.
fn multipart(pasted: &str, file: &str) -> (String, String) {
    let boundary = "XhosterBoundaryX";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"toml\"\r\n\r\n{pasted}\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"stack.toml\"\r\n\
         Content-Type: application/toml\r\n\r\n{file}\r\n--{boundary}--\r\n"
    );
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[tokio::test]
async fn stacks_list_shows_cards_with_services_revision_and_last_deploy() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app.get("/stacks", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains(&format!(
            r#"<a href="/stacks" aria-current="page">{}Stacks <span class="nav-count">1</span></a>"#,
            crate::web::Icon::Stacks
        )),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            r#"<a class="button primary" href="/apply">{}New stack</a>"#,
            crate::web::Icon::New
        )),
        "the header's action: {html}"
    );
    assert!(
        html.contains(r#"<h2><a href="/stacks/notes">notes</a></h2>"#),
        "{html}"
    );
    let hash = ContentHash::of(NOTES).to_string();
    assert!(html.contains(&format!(
        r#"rev 1 <code title="{hash}">{}</code>"#,
        &hash[..12]
    )));
    // The services as chips: a state other than ok in words, the digest in the tooltip and for
    // screen readers.
    assert!(
        html.contains(&format!(
            r#"<li class="chip" title="generation 2, sha256:{}, health unknown"><span class="dot dot-ok" aria-hidden="true"></span>api<span class="visually-hidden">: ok, generation 2, sha256:{}, health unknown</span></li>"#,
            "01".repeat(6),
            "01".repeat(6)
        )),
        "{html}"
    );
    assert!(
        html.contains(r#"<li class="chip" title="not deployed"><span class="dot dot-neutral" aria-hidden="true"></span>storage <span class="chip-state chip-state-neutral">stopped</span><span class="visually-hidden">: not deployed</span></li>"#),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            r##"<a href="/stacks/notes/deployments/3">#3</a> <span class="pill pill-ok">{}Live</span> <span class="muted">apply</span>"##,
            crate::web::Icon::Ok
        )),
        "{html}"
    );
    assert!(html.contains("notes.example.com</li>"), "{html}");
    assert!(
        html.contains(r#"<a class="add-stack" href="/apply">"#),
        "{html}"
    );
    // The failed-deploy flag (ADR-0049), escaped.
    assert!(
        html.contains(&format!(
            r#"<span class="pill pill-fail pill-long">{}revision 1: deploy failed: &#60;pull&#62; denied</span>"#,
            crate::web::Icon::Failed
        )),
        "{html}"
    );
}

#[tokio::test]
async fn the_stack_header_has_the_status_deployments_actions_and_tabs() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<nav class="crumb" aria-label="Breadcrumb"><a href="/stacks">~/stacks</a>/notes</nav>"#),
        "{html}"
    );
    // The flag of the failed deploy makes the stack failed; one of two services is up.
    assert!(
        html.contains(&format!(
            r#"<span class="pill pill-fail">{}failed · 1/2 up</span>"#,
            crate::web::Icon::Failed
        )),
        "{html}"
    );
    let hash = ContentHash::of(NOTES).to_string();
    assert!(
        html.contains(&format!(
            r#"rev 1 <code title="{hash}">{}</code> · applied <time"#,
            &hash[..12]
        )),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            r##"<a href="/stacks/notes/deployments/3">#3</a> <span class="pill pill-ok">{}Live</span> · apply"##,
            crate::web::Icon::Ok
        )),
        "{html}"
    );
    // Redeploy redeploys the last live deployment, through the history's endpoint and confirm.
    assert!(
        html.contains(r##"hx-post="/stacks/notes/deployments/3/redeploy" hx-target="#stack-action-result" hx-confirm="Redeploy deployment #3? Its digests"##),
        "{html}"
    );
    assert!(
        html.contains(
            r##"hx-post="/stacks/notes/reresolve/preview" hx-target="#reresolve-result""##
        ),
        "{html}"
    );
    for (href, label, current) in [
        ("/stacks/notes", "overview", true),
        ("/stacks/notes/logs", "logs", false),
        ("/stacks/notes/secrets", "environment", false),
        ("/stacks/notes/deployments", "deployments", false),
    ] {
        let current = if current {
            r#"aria-selected="true" aria-current="page""#
        } else {
            r#"aria-selected="false""#
        };
        assert!(
            html.contains(&format!(
                r#"<a role="tab" href="{href}" {current}>{label}</a>"#
            )),
            "{label}: {html}"
        );
    }
    assert!(html.contains(r#"<div role="tablist">"#), "{html}");

    assert!(
        html.contains(&format!("{}Redeploy</button>", crate::web::Icon::Redeploy)),
        "{html}"
    );
    // Third-party tags are pinned; the services card says what re-resolving does.
    assert!(
        html.contains("Re-resolve tags previews newer ones"),
        "{html}"
    );

    // The logs tab leads to the containers until it has its own page; an unknown stack is a 404.
    let logs = app.get("/stacks/notes/logs", Some(&browser.cookie)).await;
    assert_eq!(logs.status(), StatusCode::SEE_OTHER);
    assert_eq!(logs.headers()[header::LOCATION], "/containers");
    let missing = app.get("/stacks/nope/logs", Some(&browser.cookie)).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn redeploy_falls_back_to_the_latest_live_deployment_in_the_history() {
    let app = TestApp::new().await;
    {
        let mut stacks = app.stacks.stacks.lock().unwrap();
        stacks[0].last = Some(deployment(5, DeploymentState::Failed));
        stacks[0].flag = None;
    }
    *app.stacks.deployments.lock().unwrap() = [
        (2, DeploymentState::Live),
        (4, DeploymentState::Superseded),
        (5, DeploymentState::Failed),
    ]
    .map(|(id, state)| (deployment(id, state), Vec::new()))
    .into();
    let browser = app.sign_in().await;
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"hx-post="/stacks/notes/deployments/2/redeploy""#),
        "{html}"
    );
    // Redeploying #2 rolls back past the latest deployment, #5: the button and confirm say so.
    assert!(
        html.contains(&format!(
            "{}Redeploy #2</button>",
            crate::web::Icon::Redeploy
        )),
        "{html}"
    );
    assert!(
        html.contains(r#"hx-confirm="Roll back to deployment #2? The latest deployment, #5, is not live; #2&#39;s digests and stack revision become the running ones.""#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span><a href="/stacks/notes/deployments/5">#5</a>"#),
        "{html}"
    );

    // While #6 runs and #7, the latest, waits, the redeploy queues behind #6 and replaces #7.
    {
        let mut stacks = app.stacks.stacks.lock().unwrap();
        stacks[0].last = Some(deployment(7, DeploymentState::Queued));
        stacks[0].active = Some((DeploymentId::from(6), DeploymentState::Pulling));
        stacks[0].queued = Some(DeploymentId::from(7));
    }
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"hx-confirm="Redeploy deployment #2? It queues behind the running deployment #6 and replaces the queued deployment #7; then #2&#39;s digests and stack revision become the running ones.""#),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/stacks/notes/deployments/6">#6</a> running: Pulling"#),
        "{html}"
    );
    assert!(html.contains("<span>#7 queued</span>"), "{html}");
    // The latest deployment is the queued one, so it is not listed a second time.
    assert!(!html.contains("/stacks/notes/deployments/7"), "{html}");

    // Nothing ever went live: no Redeploy button.
    app.stacks.deployments.lock().unwrap().clear();
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(!html.contains("/redeploy"), "{html}");
}

#[tokio::test]
async fn the_danger_zone_names_what_goes_and_lists_the_volumes() {
    let app = TestApp::new().await;
    let toml = format!("{NOTES}\n[services.api.volumes]\ndata = \"/data\"\n");
    app.stacks.stacks.lock().unwrap()[0].revision = revision("notes", 1, &toml);
    let browser = app.sign_in().await;
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    let danger = &html[html
        .find(r#"class="card danger-zone""#)
        .expect("a danger zone")..];
    assert!(
        danger.contains("(one per service: api, storage), its network and its routes for <span class=\"mono\">notes.example.com</span>."),
        "{danger}"
    );
    assert!(
        danger.contains("revisions and deployment history are kept"),
        "{danger}"
    );
    assert!(
        danger.contains(
            r#"<li><code>hoster-notes-data</code> <span class="muted">→ api:/data</span></li>"#
        ),
        "{danger}"
    );
    assert!(danger.contains(r#"name="delete_volumes""#), "{danger}");
    assert!(danger.contains("unless you tick the box"), "{danger}");
    assert!(danger.contains(r#"name="confirm" required"#), "{danger}");
    assert!(
        danger.contains(r#"hx-post="/stacks/notes/remove""#),
        "{danger}"
    );
    // It is the last section of the overview.
    assert!(!danger.contains("<section"), "{danger}");

    // The box deletes the volumes of every revision, so those only earlier ones declared are
    // listed too.
    *app.stacks.earlier_volumes.lock().unwrap() = vec!["hoster-notes-pgdata".into()];
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<li><code>hoster-notes-pgdata</code> <span class="muted">from an earlier revision</span></li>"#),
        "{html}"
    );

    // Without volumes there is nothing to tick.
    app.stacks.earlier_volumes.lock().unwrap().clear();
    app.stacks.stacks.lock().unwrap()[0].revision = revision("notes", 1, NOTES);
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains("No revision of the stack declares a volume."),
        "{html}"
    );
    assert!(!html.contains(r#"name="delete_volumes""#), "{html}");
    assert!(!html.contains("unless you tick the box"), "{html}");
}

#[tokio::test]
async fn read_only_pages_need_a_session_but_no_javascript() {
    let app = TestApp::new().await;
    let response = app.get("/stacks", None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let browser = app.sign_in().await;
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(html.contains("<h1>notes</h1>"), "{html}");
    // Only the CI-built service gets a digest field.
    assert!(html.contains(r#"name="digest.api""#), "{html}");
    assert!(!html.contains(r#"name="digest.storage""#), "{html}");
    assert!(html.contains(r#"hx-post="/stacks/notes/deploy""#), "{html}");
    assert!(html.contains(r#"hx-post="/stacks/notes/remove""#), "{html}");

    let missing = app.get("/stacks/nope", Some(&browser.cookie)).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(
        body(missing)
            .await
            .contains("There is no stack called nope.")
    );

    let apply = body(app.get("/apply", Some(&browser.cookie)).await).await;
    assert!(
        apply.contains(r#"enctype="multipart/form-data""#),
        "{apply}"
    );
    assert!(apply.contains(r#"hx-post="/apply/preview""#), "{apply}");
}

#[tokio::test]
async fn preview_reports_validation_errors_with_line_numbers() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let bad = "version = 1\nname = \"notes\"\n\n[services.api]\nimage = \"ghcr.io/acme/api\"\nbogus = true\n";
    let response = app
        .send(post(
            "/apply/preview",
            &browser,
            FORM,
            encoded(&[("toml", bad)]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let html = body(response).await;
    assert!(html.contains("The stack file is invalid."), "{html}");
    assert!(html.contains("line 6"), "{html}");
    assert!(
        html.contains(r#"<pre class="problem-source">bogus = true</pre>"#),
        "{html}"
    );
    assert!(
        app.stacks
            .calls()
            .iter()
            .all(|c| !matches!(c, Call::Apply(_)))
    );
}

#[tokio::test]
async fn preview_shows_the_diff_pins_and_services_to_redeploy() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let edited = NOTES.replace("port = 8080", "port = 9090");
    // Browsers send form text with CRLF line endings.
    let pasted = edited.replace('\n', "\r\n");
    let response = app
        .send(post(
            "/apply/preview",
            &browser,
            FORM,
            encoded(&[("toml", &pasted)]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert_eq!(app.stacks.calls(), [Call::Preview(edited.clone())]);
    assert!(html.contains("Replaces the active"), "{html}");
    assert!(html.contains(r#"<ul class="redeploy">"#), "{html}");
    assert!(html.contains("<li>api</li>"), "{html}");
    assert!(
        html.contains(r#"<td class="diff-sign">-</td><td class="diff-text">port = 8080</td>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<td class="diff-sign">+</td><td class="diff-text">port = 9090</td>"#),
        "{html}"
    );
    assert!(
        html.contains(&format!(r#"<code class="wrap">{}</code>"#, digest(0xbb))),
        "{html}"
    );
    assert!(html.contains("Apply and redeploy 1 service<"), "{html}");
    // The confirm form carries the file, escaped.
    assert!(html.contains(r#"<input type="hidden" name="toml" value="version = 1"#));
    assert!(html.contains("name = &#34;notes&#34;"), "{html}");
}

#[tokio::test]
async fn preview_takes_an_uploaded_file_over_the_pasted_text() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let (content_type, form) = multipart("ignored", NOTES);
    let response = app
        .send(post("/apply/preview", &browser, &content_type, form))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(app.stacks.calls(), [Call::Preview(NOTES.into())]);

    let (content_type, form) = multipart("", "");
    let response = app
        .send(post("/apply/preview", &browser, &content_type, form))
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body(response).await.contains("Paste the stack file"));
}

#[tokio::test]
async fn confirming_applies_and_sends_the_browser_to_the_deployment() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app
        .send(post("/apply", &browser, FORM, encoded(&[("toml", NOTES)])))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["hx-redirect"],
        "/stacks/notes/deployments/7"
    );
    assert_eq!(app.stacks.calls(), [Call::Apply(NOTES.into())]);

    // Nothing changes: applied, but nothing to follow.
    app.stacks.changed.lock().unwrap().clear();
    let response = app
        .send(post("/apply", &browser, FORM, encoded(&[("toml", NOTES)])))
        .await;
    assert!(!response.headers().contains_key("hx-redirect"));
    assert!(body(response).await.contains("Nothing to redeploy"));
}

#[tokio::test]
async fn forms_need_the_csrf_token() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let request = Request::post("/apply")
        .header(header::COOKIE, &browser.cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, FORM)
        .body(Body::from(encoded(&[("toml", NOTES)])))
        .unwrap();
    assert_eq!(app.send(request).await.status(), StatusCode::FORBIDDEN);
    assert!(app.stacks.calls().is_empty());
}

#[tokio::test]
async fn manual_deploy_sends_the_filled_in_digests_and_shows_the_result() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let form = encoded(&[("digest.api", &format!(" {} ", digest(2)))]);
    let response = app
        .send(post("/stacks/notes/deploy", &browser, FORM, form.clone()))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("Accepted: deployment 8 is Received."),
        "{html}"
    );
    assert!(
        html.contains(r#"href="/stacks/notes/deployments/8""#),
        "{html}"
    );
    assert_eq!(
        app.stacks.calls(),
        [Call::Deploy(
            "notes".into(),
            DigestSet::from([("api".into(), digest(2))])
        )]
    );

    *app.stacks.deploy_state.lock().unwrap() = DeploymentState::Noop;
    let html = body(
        app.send(post("/stacks/notes/deploy", &browser, FORM, form))
            .await,
    )
    .await;
    assert!(html.contains("Noop: these digests already run"), "{html}");
    assert!(html.contains("flash-info"), "{html}");

    let empty = app
        .send(post(
            "/stacks/notes/deploy",
            &browser,
            FORM,
            encoded(&[("digest.api", "")]),
        ))
        .await;
    assert_eq!(empty.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let third_party = app
        .send(post(
            "/stacks/notes/deploy",
            &browser,
            FORM,
            encoded(&[("digest.storage", &digest(3))]),
        ))
        .await;
    assert_eq!(third_party.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body(third_party)
            .await
            .contains("does not use a CI-built image")
    );
}

#[tokio::test]
async fn remove_needs_the_stack_name_typed() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app
        .send(post(
            "/stacks/notes/remove",
            &browser,
            FORM,
            encoded(&[("confirm", "note"), ("delete_volumes", "on")]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body(response)
            .await
            .contains("Type the stack&#39;s name, notes")
    );
    assert!(app.stacks.calls().is_empty(), "nothing removed");

    let response = app
        .send(post(
            "/stacks/notes/remove",
            &browser,
            FORM,
            encoded(&[("confirm", "notes")]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("Removed stack notes and its 2 containers."),
        "{html}"
    );
    assert!(html.contains("Its volumes were kept."), "{html}");

    let response = app
        .send(post(
            "/stacks/notes/remove",
            &browser,
            FORM,
            encoded(&[("confirm", "notes"), ("delete_volumes", "on")]),
        ))
        .await;
    assert!(
        body(response)
            .await
            .contains("Deleted volumes: hoster-notes-data.")
    );
    assert_eq!(
        app.stacks.calls(),
        [
            Call::Remove(RemoveStack::keeping_volumes("notes")),
            Call::Remove(RemoveStack::deleting_volumes("notes", "notes")),
        ]
    );
}

#[tokio::test]
async fn the_deployment_page_follows_a_running_deployment_until_it_ends() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let event = DeploymentEvent {
        from: Some(DeploymentState::Received),
        to: DeploymentState::Pulling,
        message: Some("pulling <api>".into()),
        at: Timestamp::now(),
    };
    app.stacks
        .deployments
        .lock()
        .unwrap()
        .push((deployment(7, DeploymentState::Pulling), vec![event.clone()]));

    let html = body(
        app.get("/stacks/notes/deployments/7", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(html.contains("<h1>Deployment 7 of"), "{html}");
    assert!(
        html.contains(
            r#"hx-sse:connect="/stacks/notes/deployments/7/events" hx-swap="innerHTML" hx-sse:close="end""#
        ),
        "{html}"
    );
    assert!(html.contains("pulling &#60;api&#62;"), "{html}");

    app.stacks.deployments.lock().unwrap()[0].0 = deployment(7, DeploymentState::Live);
    let partial = body(
        app.get("/stacks/notes/deployments/7/status", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(
        partial.starts_with(r#"<section id="deployment-status">"#),
        "{partial}"
    );
    assert!(
        !partial.contains("hx-sse"),
        "the partial never opens a stream itself"
    );
    assert!(
        partial.contains(&format!(
            r#"<span class="pill pill-ok">{}Live</span>"#,
            crate::web::Icon::Ok
        )),
        "{partial}"
    );

    for path in [
        "/stacks/other/deployments/7",
        "/stacks/notes/deployments/99",
        "/stacks/notes/deployments/x",
    ] {
        let response = app.get(path, Some(&browser.cookie)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn a_deployment_before_switching_can_be_cancelled_from_its_page() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    app.stacks
        .deployments
        .lock()
        .unwrap()
        .push((deployment(7, DeploymentState::Pulling), Vec::new()));

    let html = body(
        app.get("/stacks/notes/deployments/7", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(
        html.contains(
            r##"hx-post="/stacks/notes/deployments/7/cancel" hx-target="#cancel-result""##
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"<div id="cancel-result" aria-live="polite"></div>"#),
        "{html}"
    );

    let response = app
        .send(post(
            "/stacks/notes/deployments/7/cancel",
            &browser,
            FORM,
            String::new(),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body(response).await.contains("Cancelling deployment 7"),
        "the answer says what happens"
    );
    assert_eq!(
        app.stacks.calls(),
        [Call::Cancel("notes".into(), DeploymentId::from(7))]
    );

    // From `Switching` on, there is no button, and a late request is refused.
    app.stacks.deployments.lock().unwrap()[0].0 = deployment(7, DeploymentState::Switching);
    let partial = body(
        app.get("/stacks/notes/deployments/7/status", Some(&browser.cookie))
            .await,
    )
    .await;
    assert!(!partial.contains("/cancel"), "{partial}");
    let late = app
        .send(post(
            "/stacks/notes/deployments/7/cancel",
            &browser,
            FORM,
            String::new(),
        ))
        .await;
    assert_eq!(late.status(), StatusCode::CONFLICT);
    assert!(
        body(late)
            .await
            .contains("cannot be cancelled: it is Switching"),
        "the refusal names the state"
    );

    let missing = app
        .send(post(
            "/stacks/notes/deployments/x/cancel",
            &browser,
            FORM,
            String::new(),
        ))
        .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------------------------

#[test]
fn file_diff_has_hunks_with_line_numbers_and_context() {
    let old = (1..=20).map(|n| format!("line {n}\n")).collect::<String>();
    let new = old
        .replace("line 2\n", "line two\n")
        .replace("line 18\n", "");
    let diff = FileDiff::between(&old, &new);
    assert_eq!(diff.hunks.len(), 2, "{diff:?}");
    let first = &diff.hunks[0];
    assert_eq!(
        first
            .iter()
            .filter(|l| l.kind != "same")
            .map(|l| (l.sign, l.old, l.new, l.text.as_str()))
            .collect::<Vec<_>>(),
        [
            ('-', Some(2), None, "line 2"),
            ('+', None, Some(2), "line two"),
        ]
    );
    assert_eq!(
        first.last().unwrap().text,
        "line 5",
        "three lines of context"
    );
    assert!(FileDiff::between(&old, &old).is_empty());
    // A new stack: every line is added.
    let created = FileDiff::between("", "a\nb\n");
    assert!(created.hunks[0].iter().all(|l| l.sign == '+'));
}

#[test]
fn short_digests_keep_the_algorithm() {
    assert_eq!(
        ShortDigest(&digest(0xab)).to_string(),
        format!("sha256:{}", "ab".repeat(6))
    );
    assert_eq!(ShortDigest("abc").to_string(), "abc");
}

#[test]
fn orchestrator_errors_map_to_status_codes_and_messages() {
    let invalid = Stack::parse("version = 1\nname = \"Bad\"\n").unwrap_err();
    let outcome =
        Outcome::from(OrchestratorError::Invalid(invalid)).quoting("version = 1\nname = \"Bad\"\n");
    assert_eq!(outcome.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!outcome.problems.is_empty());
    assert!(
        outcome.problems.iter().any(|p| p.source.is_some()),
        "{outcome:?}"
    );

    let busy = Outcome::from(OrchestratorError::StackBusy("notes".into()));
    assert_eq!(busy.status, StatusCode::CONFLICT);
    assert_eq!(busy.message, "Stack notes has an active deployment.");
    assert_eq!(
        Outcome::from(OrchestratorError::UnknownStack("x".into())).status,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn preview_of_a_new_stack_says_so_and_lists_no_redeploy() {
    let preview = ApplyPreview {
        stack: "notes".into(),
        current: None,
        warnings: Vec::new(),
        pins: Vec::new(),
        changed: Vec::new(),
    };
    let html = PreviewView::new(preview, NOTES.into()).render().unwrap();
    assert!(
        html.contains("Creates the new stack <strong>notes</strong>."),
        "{html}"
    );
    assert!(
        html.contains("No service: every service already runs"),
        "{html}"
    );
    assert!(html.contains(">Apply</button>"), "{html}");
}

// ---------------------------------------------------------------------------------------------
// Re-resolve tags (ADR-0025, ADR-0049)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stack_page_offers_reresolve_and_secrets() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"hx-post="/stacks/notes/reresolve/preview""#),
        "{html}"
    );
    assert!(html.contains(r#"href="/stacks/notes/secrets""#), "{html}");
}

#[tokio::test]
async fn reresolve_previews_the_moved_digests_then_applies_the_reviewed_revision() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app
        .send(post("/stacks/notes/reresolve/preview", &browser, FORM, ""))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(html.contains(r#"<tr class="changes">"#), "{html}");
    assert!(html.contains(&digest(0xbb)), "{html}");
    assert!(html.contains("<li>storage</li>"), "{html}");
    assert!(
        html.contains(r#"<input type="hidden" name="revision" value="1">"#),
        "{html}"
    );
    assert!(
        html.contains("Apply and redeploy 1 service</button>"),
        "{html}"
    );
    // The preview changes nothing.
    assert_eq!(app.stacks.calls(), [Call::ReresolvePreview("notes".into())]);

    let response = app
        .send(post(
            "/stacks/notes/reresolve",
            &browser,
            FORM,
            encoded(&[("revision", "1")]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        crate::web::auth::tests::header(&response, "hx-redirect"),
        "/stacks/notes/deployments/9"
    );
    assert!(
        app.stacks
            .calls()
            .contains(&Call::Reresolve("notes".into(), 1))
    );

    // A confirmation for a revision that is no longer active is refused.
    let response = app
        .send(post(
            "/stacks/notes/reresolve",
            &browser,
            FORM,
            encoded(&[("revision", "4")]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(body(response).await.contains("review the change again"));
}

#[tokio::test]
async fn reresolve_with_unchanged_tags_offers_nothing_to_apply() {
    let app = TestApp::new().await;
    *app.stacks.tags_moved.lock().unwrap() = false;
    let browser = app.sign_in().await;
    let html = body(
        app.send(post("/stacks/notes/reresolve/preview", &browser, FORM, ""))
            .await,
    )
    .await;
    assert!(html.contains("Every tag still resolves"), "{html}");
    assert!(!html.contains("<button"), "{html}");

    let response = app
        .send(post(
            "/stacks/notes/reresolve",
            &browser,
            FORM,
            encoded(&[("revision", "1")]),
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(html.contains("flash-info"), "{html}");
    assert!(html.contains("nothing changed"), "{html}");
}

#[tokio::test]
async fn reresolve_needs_the_csrf_token() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let request = Request::post("/stacks/notes/reresolve")
        .header(header::COOKIE, &browser.cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, FORM)
        .body(Body::from("revision=1"))
        .unwrap();
    assert_eq!(app.send(request).await.status(), StatusCode::FORBIDDEN);
    assert!(app.stacks.calls().is_empty());
}
