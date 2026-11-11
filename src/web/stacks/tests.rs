//! The stack pages against a fake orchestrator, through the whole app (sessions, CSRF), and
//! their templates rendered on their own.

use std::sync::Mutex;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt as _;

use super::*;
use crate::health::HealthStats;
use crate::orchestrator::{PinChange, RestartStats, ServiceState, ServiceStatus, Wave};
use crate::runtime::VolumeMount;
use crate::stack::Stack;
use crate::store::{ContentHash, RevisionId, StackRevision};
use crate::web::auth::tests::{Browser, ORIGIN, TestApp};

pub const NOTES: &str = r#"version = 1
name = "notes"

[services.api]
image = "ghcr.io/acme/notes-api"
env = { RUST_LOG = "info,notes=debug" }
secrets = { DATABASE_URL = "notes/db", S3_SECRET_KEY = "notes/missing" }

[[services.api.routes]]
host = "notes.example.com"
port = 8080

[services.storage]
image = "docker.io/rustfs/rustfs:1.0.0"
env = { RUSTFS_ADDRESS = ":9000" }
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
pub(crate) fn notes_status() -> StackStatus {
    StackStatus {
        name: "notes".into(),
        committed: None,
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
                waiting_for: Vec::new(),
            },
            ServiceStatus {
                name: "storage".into(),
                image: "docker.io/rustfs/rustfs:1.0.0".into(),
                ci_built: false,
                state: ServiceState::NotDeployed,
                restarts: RestartStats::default(),
                health: HealthStats::default(),
                waiting_for: Vec::new(),
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
    Check(String),
    Apply(String),
    Deploy(String, DigestSet),
    Webhook(Box<WebhookDeploy>),
    Remove(RemoveStack),
    ReresolvePreview(String),
    Reresolve(String, u32),
    UpdatePreview(String, Vec<UpdateChoice>),
    Update(String, Vec<UpdateChoice>, u32),
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
    /// The start order a preview reports.
    pub start_order: Mutex<Vec<Wave>>,
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
    /// Who did each operator action (apply, re-resolve, remove, secrets), in order.
    pub operators: Mutex<Vec<Operator>>,
    /// The event log, in any order.
    pub log: Mutex<Vec<crate::store::Event>>,
    /// What the live check reports for a file that parses, besides the parser's warnings.
    pub check: Mutex<CheckReport>,
    /// The updates on offer, by stack.
    pub updates: Mutex<BTreeMap<String, StackUpdates>>,
}

impl Default for FakeStacks {
    fn default() -> Self {
        Self {
            stacks: Mutex::new(vec![notes_status()]),
            deployments: Mutex::new(Vec::new()),
            changed: Mutex::new(vec!["api".into()]),
            start_order: Mutex::new(Vec::new()),
            deploy_state: Mutex::new(DeploymentState::Received),
            secrets: Mutex::new(notes_secrets()),
            tags_moved: Mutex::new(true),
            calls: Mutex::new(Vec::new()),
            foreign: Mutex::new(Vec::new()),
            rejections: Mutex::new(Vec::new()),
            events: broadcast::channel(16).0,
            earlier_volumes: Mutex::new(Vec::new()),
            operators: Mutex::new(Vec::new()),
            log: Mutex::new(Vec::new()),
            check: Mutex::new(CheckReport::default()),
            updates: Mutex::new(BTreeMap::new()),
        }
    }
}

impl FakeStacks {
    /// `toml` with the tags the offers `choices` select written in, or the error the real
    /// orchestrator gives for a choice not on offer.
    fn updated_file(
        &self,
        stack: &str,
        toml: &str,
        choices: &[UpdateChoice],
    ) -> Result<String, OrchestratorError> {
        let updates = self.updates.lock().unwrap();
        let offers = updates.get(stack).cloned().unwrap_or_default();
        let chosen = offers
            .chosen(choices)
            .map_err(OrchestratorError::NoUpdate)?;
        let mut file = toml.to_owned();
        for offer in chosen.values() {
            file = file.replace(&format!(":{}\"", offer.from), &format!(":{}\"", offer.to));
        }
        Ok(file)
    }

    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    /// The operator action was done `by`.
    fn acted(&self, by: &Operator) {
        self.operators.lock().unwrap().push(by.clone());
    }

    pub fn operators(&self) -> Vec<Operator> {
        self.operators.lock().unwrap().clone()
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// Makes `number` the active revision of `stack`, its file unchanged.
    pub fn set_revision(&self, stack: &str, number: u32) {
        for status in self.stacks.lock().unwrap().iter_mut() {
            if status.name == stack {
                status.revision.revision = number;
            }
        }
    }

    pub fn current(&self, stack: &str) -> Option<StackStatus> {
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
            start_order: self.start_order.lock().unwrap().clone(),
        })
    }

    async fn check_file(&self, toml: &str) -> Result<CheckReport, OrchestratorError> {
        self.record(Call::Check(toml.into()));
        match Stack::parse(toml) {
            Ok(parsed) => {
                let mut report = self.check.lock().unwrap().clone();
                report.warnings.extend(parsed.warnings);
                Ok(report)
            }
            Err(invalid) => Ok(CheckReport {
                errors: invalid.diagnostics().to_vec(),
                warnings: Vec::new(),
            }),
        }
    }

    async fn apply(&self, toml: &str, by: &Operator) -> Result<ApplyOutcome, OrchestratorError> {
        self.record(Call::Apply(toml.into()));
        self.acted(by);
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

    async fn remove(
        &self,
        request: RemoveStack,
        by: &Operator,
    ) -> Result<RemoveOutcome, OrchestratorError> {
        self.record(Call::Remove(request.clone()));
        self.acted(by);
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

    /// Like the store: the query's events, newest first, a page of at most its limit from its
    /// cursor, with where the next page starts.
    async fn event_page(&self, query: &EventQuery) -> Result<EventPage, OrchestratorError> {
        let mut events: Vec<_> = self
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|e| query.stack.is_none() || e.stack == query.stack)
            .filter(|e| query.kinds.is_empty() || query.kinds.contains(&e.kind))
            .filter(|e| e.severity >= query.min_severity)
            .filter(|e| query.before.is_none_or(|cursor| e.is_older_than(cursor)))
            .cloned()
            .collect();
        events.sort_by_key(|e| std::cmp::Reverse((e.at, e.id)));
        let limit = query.limit.clamp(1, EventQuery::MAX) as usize;
        let next = (events.len() > limit).then(|| events[limit - 1].cursor());
        events.truncate(limit);
        Ok(EventPage { events, next })
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
            start_order: Vec::new(),
        })
    }

    async fn reresolve(
        &self,
        stack: &str,
        expected: u32,
        by: &Operator,
    ) -> Result<Option<ApplyOutcome>, OrchestratorError> {
        self.record(Call::Reresolve(stack.into(), expected));
        self.acted(by);
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

    async fn stack_updates(&self, stack: &str) -> Result<StackUpdates, OrchestratorError> {
        self.current(stack)
            .ok_or_else(|| OrchestratorError::UnknownStack(stack.into()))?;
        Ok(self
            .updates
            .lock()
            .unwrap()
            .get(stack)
            .cloned()
            .unwrap_or_default())
    }

    async fn all_updates(&self) -> Result<BTreeMap<String, StackUpdates>, OrchestratorError> {
        Ok(self.updates.lock().unwrap().clone())
    }

    /// The active revision with each chosen service's image tag replaced, as the real one
    /// would write it.
    async fn update_preview(
        &self,
        stack: &str,
        choices: &[UpdateChoice],
    ) -> Result<UpdatePreview, OrchestratorError> {
        self.record(Call::UpdatePreview(stack.into(), choices.to_vec()));
        let status = self
            .current(stack)
            .ok_or_else(|| OrchestratorError::UnknownStack(stack.into()))?;
        let file = self.updated_file(stack, &status.revision.toml, choices)?;
        Ok(UpdatePreview {
            preview: ApplyPreview {
                stack: stack.into(),
                current: Some(status.revision.clone()),
                warnings: Vec::new(),
                pins: Vec::new(),
                changed: self.changed.lock().unwrap().clone(),
                start_order: Vec::new(),
            },
            revision: status.revision,
            file,
        })
    }

    async fn update(
        &self,
        stack: &str,
        choices: &[UpdateChoice],
        expected: u32,
        by: &Operator,
    ) -> Result<ApplyOutcome, OrchestratorError> {
        self.record(Call::Update(stack.into(), choices.to_vec(), expected));
        self.acted(by);
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
        let file = self.updated_file(stack, &status.revision.toml, choices)?;
        Ok(ApplyOutcome {
            revision: revision(stack, expected + 1, &file),
            warnings: Vec::new(),
            changed: self.changed.lock().unwrap().clone(),
            deployment: Some(DeployOutcome {
                deployment: DeploymentId::from(9),
                state: DeploymentState::Validating,
            }),
        })
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
        by: &Operator,
    ) -> Result<SecretChange, OrchestratorError> {
        self.acted(by);
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
        by: &Operator,
    ) -> Result<SecretChange, OrchestratorError> {
        self.record(Call::DeleteSecret(stack.into(), name.into()));
        self.acted(by);
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
            r##"<a href="/stacks/notes/deployments/3">#3</a> <span class="pill pill-ok">{}live</span> <span class="muted">apply</span>"##,
            crate::web::Icon::Ok
        )),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="https://notes.example.com" target="_blank" rel="noopener noreferrer" title="Open https://notes.example.com in a new tab"><span aria-hidden="true">↗ </span>notes.example.com</a></li>"#),
        "{html}"
    );
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
            r##"<a href="/stacks/notes/deployments/3">#3</a> <span class="pill pill-ok">{}live</span> · apply"##,
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
        ("/stacks/notes/environment", "environment", false),
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

    // Without containers, the logs tab says there is no log yet; an unknown stack is a 404.
    let logs = app.get("/stacks/notes/logs", Some(&browser.cookie)).await;
    assert_eq!(logs.status(), StatusCode::OK);
    let html = body(logs).await;
    assert!(html.contains("api has no container yet"), "{html}");
    assert!(!html.contains("hx-sse:connect"), "{html}");
    assert!(!html.contains(r#"class="log-options""#), "{html}");
    let missing = app.get("/stacks/nope/logs", Some(&browser.cookie)).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

/// Starts `hoster-<stack>-<service>-g<generation>` in the app's fake runtime, publishing
/// `ports` (host port, container port), up for `up`.
pub(crate) async fn start_container(
    app: &TestApp,
    stack: &str,
    service: &str,
    generation: u32,
    ports: &[(u16, u16)],
    up: std::time::Duration,
) -> crate::runtime::ContainerId {
    use crate::runtime::{
        ContainerRuntime, ContainerSpec, NetworkSpec, PortBinding, ServiceGeneration,
    };
    let generation = ServiceGeneration {
        stack: stack.parse().unwrap(),
        service: service.parse().unwrap(),
        generation,
    };
    let image = format!("ghcr.io/acme/{service}@{}", digest(1));
    let mut spec = ContainerSpec::new(&generation, DeploymentId::from(1), image.parse().unwrap());
    spec.ports = ports
        .iter()
        .map(|&(host_port, container_port)| PortBinding {
            container_port,
            host_port,
        })
        .collect();
    let runtime = &app.runtime;
    runtime
        .ensure_network(&NetworkSpec::for_stack(&generation.stack))
        .await
        .unwrap();
    runtime.pull_image(&spec.image).await.unwrap();
    let id = runtime.create_container(&spec).await.unwrap();
    runtime.start(&id).await.unwrap();
    runtime.set_started_at(&id, std::time::SystemTime::now() - up);
    id
}

#[tokio::test]
async fn the_services_table_lists_the_containers_with_ports_and_uptime() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let hour = std::time::Duration::from_secs(3_600);
    // api runs generation 2; generation 1 is still draining. A container of another stack is
    // not listed.
    start_container(&app, "notes", "api", 1, &[(20001, 8080)], 26 * hour).await;
    start_container(&app, "notes", "api", 2, &[(20004, 8080)], 2 * hour).await;
    start_container(&app, "blog", "web", 1, &[(20002, 8080)], hour).await;

    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<h2 id="services-title">Services <span class="card-count">· 2 · 2 containers</span></h2>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="mono service-name">api</span><code class="service-container"><span class="visually-hidden">container </span>hoster-notes-api-g2</code>"#),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            r#"<code class="service-digest" title="{}">sha256:{}</code>"#,
            digest(1),
            "01".repeat(6)
        )),
        "{html}"
    );
    assert!(
        html.contains(r#"<td class="mono nowrap">20004 → 8080</td>"#),
        "{html}"
    );
    assert!(html.contains(">2h 00m</span>"), "{html}");
    assert!(
        html.contains(r#"<a class="mono" href="/stacks/notes/logs?service=api">logs<span class="visually-hidden"> of api</span></a>"#),
        "{html}"
    );
    // The draining generation, muted under its service.
    assert!(
        html.contains(
            r#"<td><span aria-hidden="true">↳ </span><code>hoster-notes-api-g1</code></td>"#
        ),
        "{html}"
    );
    assert!(html.contains("<td>previous generation</td>"), "{html}");
    assert!(html.contains(">1d 2h</span>"), "{html}");
    assert!(
        html.contains(r#"href="/stacks/notes/logs?service=api&#38;container=hoster-notes-api-g1""#),
        "{html}"
    );
    // storage is not routed and not deployed.
    assert!(
        html.contains(r#"<td class="mono nowrap">internal</td>"#),
        "{html}"
    );
    assert!(html.contains(">stopped</span>"), "{html}");
    assert!(!html.contains("hoster-blog-web-g1"), "{html}");
    assert!(!html.contains("style="), "{html}");

    // The logs tab streams the running container's log, or the one asked for, and offers the
    // draining generation beside it.
    for (url, streamed) in [
        ("/stacks/notes/logs", Some("hoster-notes-api-g2")),
        (
            "/stacks/notes/logs?service=api",
            Some("hoster-notes-api-g2"),
        ),
        (
            "/stacks/notes/logs?service=api&container=hoster-notes-api-g1",
            Some("hoster-notes-api-g1"),
        ),
        (
            "/stacks/notes/logs?service=api&container=hoster-blog-web-g1",
            Some("hoster-notes-api-g2"),
        ),
        ("/stacks/notes/logs?service=storage", None),
    ] {
        let response = app.get(url, Some(&browser.cookie)).await;
        assert_eq!(response.status(), StatusCode::OK, "{url}");
        let html = body(response).await;
        match streamed {
            Some(name) => assert!(
                html.contains(&format!(
                    r#"hx-sse:connect="/containers/{name}/logs/stream?"#
                )),
                "{url}: {html}"
            ),
            None => assert!(
                html.contains("storage has no container yet"),
                "{url}: {html}"
            ),
        }
    }
    let html = body(
        app.get(
            "/stacks/notes/logs?container=hoster-notes-api-g1",
            Some(&browser.cookie),
        )
        .await,
    )
    .await;
    assert!(
        html.contains(r#"<nav class="log-choices log-containers" aria-label="Containers of api">"#),
        "{html}"
    );
    assert!(
        html.contains(r#"href="/stacks/notes/logs?service=api&#38;container=hoster-notes-api-g1&#38;tail=200&#38;since=&#38;follow=on" aria-current="true">hoster-notes-api-g1 <span class="log-chip-detail">g1 · previous generation · running</span></a>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<a class="log-chip" href="/stacks/notes/logs?service=api&#38;tail=200&#38;since=&#38;follow=on" aria-current="page"><span class="dot dot-s0" aria-hidden="true"></span>api</a>"#),
        "{html}"
    );
    // The footer says what the streamed container is, and keys the stderr tint.
    assert!(
        html.contains(r#"<code>hoster-notes-api-g1</code> <span class="log-detail">g1 · previous generation · running</span>"#),
        "{html}"
    );
    assert!(html.contains(r#"<span class="log-key">"#), "{html}");
    assert!(!html.contains("hoster-blog-web-g1"), "{html}");

    // When podman cannot be reached, the services are still shown, and the card says why.
    app.runtime.fail_always(
        crate::runtime::Operation::ListManaged,
        crate::runtime::RuntimeError::Unreachable {
            socket: "/run/podman.sock".into(),
            message: "socket gone".into(),
        },
    );
    let response = app.get("/stacks/notes", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains("Cannot list the containers: cannot reach podman at /run/podman.sock"),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="mono service-name">api</span>"#),
        "{html}"
    );
    // The logs tab still has its service chips, and says why there is no log.
    let html = body(app.get("/stacks/notes/logs", Some(&browser.cookie)).await).await;
    assert!(html.contains("flash-error"), "{html}");
    assert!(
        html.contains("Cannot list the stack&#39;s containers")
            || html.contains("Cannot list the stack&#x27;s containers")
            || html.contains("Cannot list the stack's containers"),
        "{html}"
    );
    assert!(html.contains(r#"class="log-chip""#), "{html}");
    assert!(html.contains("could not be listed"), "{html}");
    assert!(!html.contains("has no container yet"), "{html}");
}

/// ADR-0056: each service shows its `depends_on`, the health check in effect (declared or the
/// default, with its command or path) and the dependencies the reconciler waits for.
#[tokio::test]
async fn the_services_table_shows_dependencies_checks_and_waits() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let toml = format!(
        "{}\n[services.storage.health]\nexec = [\"pg_isready\", \"-U\", \"app\"]\n",
        NOTES.replace(
            "[services.storage]\n",
            "[services.storage]\ndepends_on = [\"api\"]\n"
        )
    );
    {
        let mut stacks = app.stacks.stacks.lock().unwrap();
        let notes = &mut stacks[0];
        notes.revision = revision("notes", 2, &toml);
        let storage = &mut notes.services[1];
        storage.state = ServiceState::Running {
            generation: 1,
            digest: digest(2),
        };
        storage.waiting_for = vec!["api".into()];
    }
    start_container(
        &app,
        "notes",
        "api",
        2,
        &[(20004, 8080)],
        std::time::Duration::ZERO,
    )
    .await;

    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    // storage names its dependency under its name; api depends on nothing.
    assert!(
        html.contains(r#"<span class="mono service-name">storage</span><span class="service-deps">depends on api</span>"#),
        "{html}"
    );
    assert_eq!(html.matches("service-deps").count(), 1, "{html}");
    // api gets the routed default, storage its declared exec check.
    assert!(
        html.contains(r#"<span class="service-check" title="the default for a routed service without health"><span class="visually-hidden">health check </span>GET /healthz on container port 8080 <span class="service-check-source">· default</span></span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="service-check" title="declared in health.exec"><span class="visually-hidden">health check </span>exec: pg_isready -U app <span class="service-check-source">· declared</span></span>"#),
        "{html}"
    );
    // The reconciler holds storage back until api runs and passes.
    assert!(
        html.contains(&format!(
            r#"<span class="pill pill-warn" title="stopped: the reconciler starts it once api runs and passes its check">{}waiting for api</span>"#,
            crate::web::Icon::Warning
        )),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="visually-hidden">: stopped: the reconciler starts it once api runs and passes its check</span>"#),
        "{html}"
    );
}

/// ADR-0056: notes within a state span the From and To columns and say what they are; an exec
/// probe's output is collapsed under its result.
#[tokio::test]
async fn the_deployment_page_shows_waves_checks_and_probe_output() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let at = Timestamp::now();
    let event = |from: DeploymentState, to: DeploymentState, message: &str| DeploymentEvent {
        from: Some(from),
        to,
        message: Some(message.into()),
        at,
    };
    let checking = DeploymentState::HealthChecking;
    let events = vec![
        event(
            DeploymentState::Pulling,
            DeploymentState::Starting,
            "pulled 2 images",
        ),
        event(
            DeploymentState::Starting,
            DeploymentState::Starting,
            "starting db, cache",
        ),
        event(DeploymentState::Starting, checking, ""),
        event(
            checking,
            checking,
            "checking db with exec: pg_isready -U app, declared in health.exec",
        ),
        event(
            checking,
            checking,
            r#"health probe exec: pg_isready -U app: exit 2, output "/run/postgresql:5432 - no response
<retrying>""#,
        ),
        event(
            checking,
            checking,
            "health check exec: pg_isready -U app passed 1 time(s) in a row after 3s 241ms",
        ),
        event(
            checking,
            checking,
            "db healthy after 3s 241ms; starting app",
        ),
        event(checking, checking, "health probe exec: true: exit 1"),
        event(
            checking,
            checking,
            r#"health probe exec: x: exit 1, output "bad \q escape""#,
        ),
        event(
            checking,
            checking,
            "`health.exec` cannot run: `pg_isready`: executable file not found in $PATH",
        ),
        event(
            DeploymentState::Replacing,
            DeploymentState::Replacing,
            "replacing db (stop-first)",
        ),
        event(
            DeploymentState::Replacing,
            DeploymentState::Replacing,
            "starting app (held back for db)",
        ),
        event(checking, checking, "drain timed out"),
    ];
    app.stacks
        .deployments
        .lock()
        .unwrap()
        .push((deployment(8, DeploymentState::HealthChecking), events));

    let html = body(
        app.get("/stacks/notes/deployments/8", Some(&browser.cookie))
            .await,
    )
    .await;
    // A transition keeps its From and To.
    assert!(
        html.contains(r#"<td class="mono shrink">Pulling</td><td class="mono shrink">Starting</td><td><span class="event-message">pulled 2 images</span></td>"#),
        "{html}"
    );
    // Notes span both columns and carry their kind.
    assert!(html.contains(r#"<tr class="event-note">"#), "{html}");
    assert!(
        html.contains(r#"<td class="mono shrink muted" colspan="2"><span class="visually-hidden">note in </span>Starting</td><td><span class="tag event-kind">wave</span> <span class="event-message">starting db, cache</span></td>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="tag event-kind">check</span> <span class="event-message">checking db with exec: pg_isready -U app, declared in health.exec</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="tag event-kind">wave</span> <span class="event-message">db healthy after 3s 241ms; starting app</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="tag event-kind">probe</span> <span class="event-message">health check exec: pg_isready -U app passed"#),
        "{html}"
    );
    // The probe's output is unescaped (then escaped as HTML) and collapsed.
    assert!(
        html.contains("<span class=\"tag event-kind\">probe</span> <span class=\"event-message\">health probe exec: pg_isready -U app: exit 2</span><details class=\"event-output\"><summary>output</summary><pre>/run/postgresql:5432 - no response\n&#60;retrying&#62;</pre></details>"),
        "{html}"
    );
    // Only the probe that printed something has an output; one whose tail does not read back
    // is shown as written.
    assert_eq!(html.matches("event-output").count(), 1, "{html}");
    assert!(
        html.contains(r#"<span class="tag event-kind">probe</span> <span class="event-message">health probe exec: true: exit 1</span></td>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="tag event-kind">probe</span> <span class="event-message">health probe exec: x: exit 1, output &#34;bad \q escape&#34;</span></td>"#),
        "{html}"
    );
    // Why the probes stopped is a probe event too.
    assert!(
        html.contains(r#"<span class="tag event-kind">probe</span> <span class="event-message">`health.exec` cannot run: "#),
        "{html}"
    );
    // Replacements and held-back starts are waves.
    for wave in [
        "replacing db (stop-first)",
        "starting app (held back for db)",
    ] {
        assert!(
            html.contains(&format!(
                r#"<span class="tag event-kind">wave</span> <span class="event-message">{wave}</span>"#
            )),
            "{html}"
        );
    }
    // A note of another kind gets no tag.
    assert!(
        html.contains(
            r#"HealthChecking</td><td><span class="event-message">drain timed out</span></td>"#
        ),
        "{html}"
    );
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
        html.contains(r#"<a href="/stacks/notes/deployments/6">#6</a> running: pulling"#),
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
    // Laid out as "New stack": the editor, the aside the preview fills, the diff card's slot.
    assert!(
        apply.contains(r#"<a href="/stacks">~/stacks</a>/new"#),
        "{apply}"
    );
    assert!(
        apply.contains(r#"<aside class="apply-aside" aria-label="Plan and help">"#),
        "{apply}"
    );
    assert!(
        apply.contains(r#"<div id="apply-result" class="apply-result" aria-live="polite">"#),
        "{apply}"
    );
    assert!(apply.contains(r#"<div id="apply-diff"></div>"#), "{apply}");
    assert!(
        apply.contains(r#"<button class="primary" type="submit">Check</button>"#),
        "{apply}"
    );
    // ADR-0060: the editor script enhances the textarea, which works as a plain form field
    // without it.
    assert!(
        apply.contains(&format!(
            r#"<script type="module" src="{}"></script>"#,
            crate::web::assets::Assets::get().editor.url()
        )),
        "{apply}"
    );
    assert!(
        apply.contains(r#"<textarea id="stack-toml" name="toml" rows="22" data-editor="toml""#),
        "{apply}"
    );

    // The live check posts only the editor's text, a moment after typing stops, into the panel
    // under it.
    assert!(
        apply.contains(r##"hx-post="/apply/check" hx-trigger="input changed delay:500ms" hx-target="#apply-check" hx-swap="innerMorph" hx-sync="this:replace" hx-encoding="application/x-www-form-urlencoded""##),
        "{apply}"
    );
    assert!(
        apply.contains(
            r#"<div id="apply-check" class="live-check" data-editor-for="stack-toml"></div>"#
        ),
        "{apply}"
    );

    // An empty editor starts with the skeleton (escaped), and the aside offers the snippets and
    // the field reference, closed. Without the editor script the snippets are only to copy.
    assert!(
        apply.contains("spellcheck=\"false\" autocomplete=\"off\">\n#:schema https://docs.hoster.lhelge.se/stack.schema.json"),
        "{apply}"
    );
    assert!(apply.contains("name = &#34;&#34;"), "{apply}");
    assert!(
        apply.contains(
            r#"<details class="card editor-help" id="snippets" data-snippets-for="stack-toml">"#
        ),
        "{apply}"
    );
    assert!(
        apply.contains(r#"<button type="button" class="ghost snippet-insert" data-snippet="route" data-per-service hidden>Insert<span class="visually-hidden"> Route</span></button>"#),
        "{apply}"
    );
    assert!(
        apply.contains(r#"<pre class="snippet-code" id="snippet-deploy">[deploy]"#),
        "{apply}"
    );
    assert!(
        apply.contains(r#"<details class="card editor-help" id="field-reference">"#),
        "{apply}"
    );
    assert!(
        apply.contains(r#"<h3><code>[[services.&#60;service&#62;.routes]]</code></h3>"#),
        "{apply}"
    );
    assert!(
        apply.contains(r#"<dt><code class="ref-name">image</code> <span class="ref-kind">string</span> <span class="ref-required">required</span></dt>"#),
        "{apply}"
    );
    // Code in a description stays code.
    assert!(
        apply.contains("<code>&#34;start-first&#34;</code>"),
        "{apply}"
    );

    // Other pages don't load the editor.
    let stacks = body(app.get("/stacks", Some(&browser.cookie)).await).await;
    assert!(!stacks.contains("/assets/editor."), "{stacks}");
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
    assert!(
        html.contains(r#"<section class="card plan-card preflight-fail">"#),
        "{html}"
    );
    assert!(html.contains(r#"<li class="check-fail">"#), "{html}");
    assert!(html.contains("line 6"), "{html}");
    // The diff of an earlier check no longer matches the editor.
    assert!(
        html.ends_with(r#"<div id="apply-diff" hx-swap-oob="innerHTML"></div>"#),
        "{html}"
    );
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
    // The plan: the replaced revision, the redeploy, the route that moves and the pin.
    assert!(
        html.contains(r#"replace <a href="/stacks/notes"><strong>notes revision 1</strong></a>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<li class="plan-change"><span class="plan-mark" aria-hidden="true">~</span><span class="plan-text"><span class="visually-hidden">Changes: </span>redeploy <strong>api</strong></span></li>"#),
        "{html}"
    );
    assert!(
        html.contains("route <strong>notes.example.com/ → api:9090</strong>"),
        "{html}"
    );
    assert!(
        html.contains(r#"<li class="plan-remove"><span class="plan-mark" aria-hidden="true">−</span><span class="plan-text"><span class="visually-hidden">Removes: </span>route <strong>notes.example.com/ → api:8080</strong>"#),
        "{html}"
    );
    assert!(html.contains("re-pin <strong>storage</strong>"), "{html}");
    // Pre-flight: what the preview checked.
    assert!(
        html.contains(r#"<li class="check-ok"><span class="check-mark" aria-hidden="true">✓</span><span class="check-body">stack file valid · 2 services</span></li>"#),
        "{html}"
    );
    assert!(html.contains("1 route, none taken by hoster"), "{html}");
    assert!(html.contains("1 third-party tag resolved"), "{html}");
    // The diff goes into its full-width card out of band.
    assert!(
        html.contains(r#"<div id="apply-diff" hx-swap-oob="innerHTML">"#),
        "{html}"
    );
    assert!(html.contains("against revision 1"), "{html}");
    assert!(
        html.contains(r#"<td class="diff-sign">-</td><td class="diff-text">port = 8080</td>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<td class="diff-sign">+</td><td class="diff-text">port = 9090</td>"#),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            r#"<span class="plan-detail">from <code>{}</code></span>"#,
            digest(0xaa)
        )),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            r#"<span class="plan-detail">to <code>{}</code></span>"#,
            digest(0xbb)
        )),
        "{html}"
    );
    assert!(html.contains("Apply and redeploy 1 service<"), "{html}");
    assert!(
        html.contains(r#"<a class="plan-cancel" href="/stacks">"#),
        "{html}"
    );
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

/// `text` as the templates escape it.
fn escaped(text: &str) -> String {
    askama::filters::escape(text, askama::filters::Html)
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn editing_starts_from_the_active_revision_byte_for_byte() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    // A blank first line, comments, quotes and a trailing newline all survive the editor.
    let file = format!("\n# The notes app.\n{NOTES}# end <of> file & 'all'\n");
    app.stacks.stacks.lock().unwrap()[0].revision = revision("notes", 6, &file);

    let stack = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        stack.contains(r#"<a class="button primary" href="/apply?stack=notes">"#),
        "{stack}"
    );
    assert!(
        stack.contains(r#"<a class="button" href="/apply">Apply a new file</a>"#),
        "{stack}"
    );

    let edit = body(app.get("/apply?stack=notes", Some(&browser.cookie)).await).await;
    assert!(
        edit.contains("<title>Edit notes · hoster</title>"),
        "{edit}"
    );
    assert!(edit.contains("<h1>Edit notes</h1>"), "{edit}");
    assert!(
        edit.contains(r#"<a href="/stacks">~/stacks</a>/<a href="/stacks/notes">notes</a>/edit"#),
        "{edit}"
    );
    assert!(edit.contains("The stack file of revision 6"), "{edit}");
    // HTML drops one newline after `<textarea>`, so the page always starts with one.
    assert!(
        edit.contains(&format!(
            "autocomplete=\"off\">\n{}</textarea>",
            escaped(&file)
        )),
        "{edit}"
    );
    assert!(
        edit.contains(r#"<input type="hidden" name="base_stack" value="notes">"#),
        "{edit}"
    );
    assert!(
        edit.contains(r#"<input type="hidden" name="base_revision" value="6">"#),
        "{edit}"
    );
    // The file is checked as soon as the page loads, not only once the admin types.
    assert!(
        edit.contains(r#"hx-trigger="load, input changed delay:500ms""#),
        "{edit}"
    );

    // The New stack page has no base and starts with the skeleton.
    let new = body(app.get("/apply?stack=", Some(&browser.cookie)).await).await;
    assert!(new.contains("<h1>Apply a stack file</h1>"), "{new}");
    assert!(!new.contains("base_revision"), "{new}");
    assert!(
        new.contains(r#"hx-trigger="input changed delay:500ms""#),
        "{new}"
    );

    let missing = app.get("/apply?stack=nope", Some(&browser.cookie)).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(
        body(missing)
            .await
            .contains("There is no stack called nope.")
    );
}

#[tokio::test]
async fn the_preview_warns_when_another_revision_became_active_since_the_edit_began() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    app.stacks.stacks.lock().unwrap()[0].revision = revision("notes", 7, NOTES);
    let edited = NOTES.replace("port = 8080", "port = 9090");
    let preview = async |toml: &str, base: &str, revision: &str| {
        let response = app
            .send(post(
                "/apply/preview",
                &browser,
                FORM,
                encoded(&[
                    ("toml", toml),
                    ("base_stack", base),
                    ("base_revision", revision),
                ]),
            ))
            .await;
        let status = response.status();
        let html = body(response).await;
        assert_eq!(status, StatusCode::OK, "{html}");
        html
    };
    let stale = "Revision 7 became active after you opened the editor at revision 6.";

    // Opened at 6, and 7 is active now: a warning, and the diff is against 7.
    let html = preview(&edited, "notes", "6").await;
    assert!(html.contains(stale), "{html}");
    assert!(
        html.contains(r#"<span class="card-note">1 warning</span>"#),
        "{html}"
    );
    assert!(html.contains("against revision 7"), "{html}");
    // The warning doesn't stop the apply.
    assert!(html.contains(r#"<form class="plan-confirm""#), "{html}");

    // Opened at the active revision: nothing to warn about.
    let html = preview(&edited, "notes", "7").await;
    assert!(!html.contains("became active"), "{html}");

    // Renamed in the editor: a new stack, never compared with the one it came from.
    let renamed = edited
        .replace(r#"name = "notes""#, r#"name = "jottings""#)
        .replace("\"notes/", "\"jottings/");
    let html = preview(&renamed, "notes", "6").await;
    assert!(!html.contains("became active"), "{html}");
    assert!(html.contains("create stack"), "{html}");

    // A file from the upload form carries the base as well.
    let boundary = "XhosterBoundaryX";
    let field = |name: &str, value: &str| {
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
    };
    let form = format!(
        "{}{}{}--{boundary}--\r\n",
        field("base_stack", "notes"),
        field("base_revision", "6"),
        field("toml", &edited),
    );
    let response = app
        .send(post(
            "/apply/preview",
            &browser,
            &format!("multipart/form-data; boundary={boundary}"),
            form,
        ))
        .await;
    assert!(body(response).await.contains(stale));

    // A removed stack is created again.
    app.stacks.stacks.lock().unwrap().clear();
    let html = preview(&edited, "notes", "6").await;
    assert!(
        html.contains("notes was removed after you opened the editor at revision 6."),
        "{html}"
    );
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
    // The signed-in handle is who applied it, for the event log (ADR-0055).
    assert_eq!(app.stacks.operators(), [Operator::from("octocat")]);

    // Nothing changes: applied, but nothing to follow.
    app.stacks.changed.lock().unwrap().clear();
    let response = app
        .send(post("/apply", &browser, FORM, encoded(&[("toml", NOTES)])))
        .await;
    assert!(!response.headers().contains_key("hx-redirect"));
    assert!(body(response).await.contains("Nothing to redeploy"));
}

/// A stack file may carry 1 MiB of config files (ADR-0057), which axum's default 2 MB body
/// limit would refuse once the form encodes it.
#[tokio::test]
async fn apply_forms_take_bodies_up_to_8_mib() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let big = format!("{NOTES}\n# {}\n", "x".repeat(3 * 1024 * 1024));
    for uri in ["/apply/preview", "/apply"] {
        let response = app
            .send(post(uri, &browser, FORM, encoded(&[("toml", &big)])))
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let (content_type, form) = multipart("", &big);
        let response = app.send(post(uri, &browser, &content_type, form)).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
    }
    assert_eq!(app.stacks.calls().len(), 4);

    let huge = format!("{NOTES}\n# {}\n", "x".repeat(StackFile::MAX_BODY));
    for uri in ["/apply/preview", "/apply"] {
        let response = app
            .send(post(uri, &browser, FORM, encoded(&[("toml", &huge)])))
            .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
        let (content_type, form) = multipart("", &huge);
        let response = app.send(post(uri, &browser, &content_type, form)).await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{uri}");
    }
    assert_eq!(app.stacks.calls().len(), 4);
}

/// The editor's live check (ADR-0060): a summary, every problem with its line for the gutter
/// and a link to put the caret there, and only the check called.
#[tokio::test]
async fn the_live_check_lists_problems_by_line() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let check = async |toml: &str| {
        let response = app
            .send(post(
                "/apply/check",
                &browser,
                FORM,
                encoded(&[("toml", toml)]),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        body(response).await
    };

    let clean = check(NOTES).await;
    assert!(
        clean.contains(r#"<p class="live-summary live-ok" role="status">No problems found."#),
        "{clean}"
    );
    assert!(!clean.contains("<ul"), "{clean}");

    let invalid = check(
        "version = 1\r\nname = \"Bad\"\r\n\r\n[services.a]\r\nimage = \"ghcr.io/acme/a\"\r\n",
    )
    .await;
    assert!(
        invalid.contains(r#"<p class="live-summary live-fail" role="status">1 error</p>"#),
        "{invalid}"
    );
    assert!(
        invalid.contains(r#"<li class="check-fail" data-line="2" data-severity="error">"#),
        "{invalid}"
    );
    assert!(
        invalid.contains(r##"<a class="problem-at" href="#stack-toml" data-line="2" data-column="8">line 2, column 8</a>"##),
        "{invalid}"
    );
    assert!(
        invalid.contains(r#"<code class="problem-path">name</code>"#),
        "{invalid}"
    );

    // Problems beyond the parser's, such as an unset secret, come from the orchestrator.
    app.stacks.check.lock().unwrap().warnings.push(Diagnostic {
        path: crate::stack::FieldPath::default()
            .key("services")
            .key("api"),
        location: None,
        message: "secret `notes/db` (for DB) is not set".into(),
    });
    let warned = check(NOTES).await;
    assert!(
        warned.contains(r#"<p class="live-summary live-warn" role="status">1 warning</p>"#),
        "{warned}"
    );
    // Without a line there is nothing to mark or link to.
    assert!(
        warned.contains(
            r#"<li class="check-warn"><span class="check-mark" aria-hidden="true">!</span>"#
        ),
        "{warned}"
    );
    assert!(!warned.contains("data-line"), "{warned}");

    let empty = check("  \n").await;
    assert!(
        empty.contains("The file is checked as you type."),
        "{empty}"
    );

    // Only the check ran, on the text with its line endings normalized.
    let calls = app.stacks.calls();
    assert_eq!(calls.len(), 3, "an empty editor isn't sent on: {calls:?}");
    assert!(
        calls.iter().all(|c| matches!(c, Call::Check(_))),
        "{calls:?}"
    );
    assert!(!format!("{calls:?}").contains("\\r"), "{calls:?}");

    // Like every form, it needs the CSRF token.
    let request = Request::post("/apply/check")
        .header(header::COOKIE, &browser.cookie)
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, FORM)
        .body(Body::from(encoded(&[("toml", NOTES)])))
        .unwrap();
    assert_eq!(app.send(request).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(app.stacks.calls().len(), 3);
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
    assert_eq!(app.stacks.operators(), vec![Operator::from("octocat"); 2]);
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
    assert!(
        html.contains(r#"<h2 id="deployment-title">Deployment #7</h2>"#),
        "{html}"
    );
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
        partial.starts_with(r#"<div id="deployment-status" class="deployment-status">"#),
        "{partial}"
    );
    assert!(
        !partial.contains("hx-sse"),
        "the partial never opens a stream itself"
    );
    assert!(
        partial.contains(&format!(
            r#"<span class="pill pill-ok" title="Went live">{}live</span>"#,
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
fn preview_lists_warnings_as_pre_flight_lines_with_their_source() {
    let toml = "version = 1\nname = \"wiki\"\n\n[services.web]\nimage = \"docker.io/library/nginx:1\"\nvolumes = { data = \"/data\" }\n";
    let parsed = Stack::parse(toml).unwrap();
    assert_eq!(parsed.warnings.len(), 1, "{:?}", parsed.warnings);
    let preview = ApplyPreview {
        stack: "wiki".into(),
        current: None,
        warnings: parsed.warnings,
        pins: Vec::new(),
        changed: vec!["web".into()],
        start_order: Vec::new(),
    };
    let html = PreviewView::new(preview, toml.into(), None)
        .render()
        .unwrap();
    assert!(
        html.contains(r#"<span class="card-note">1 warning</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<li class="check-warn"><span class="check-mark" aria-hidden="true">!</span><span class="check-body"><span class="visually-hidden">Warning: </span><span class="problem-where"><span class="problem-at">line 6"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<pre class="problem-source">volumes = { data = &#34;/data&#34; }</pre>"#),
        "{html}"
    );
    // A new stack: its service is new.
    assert!(
        html.contains(r#"deploy <strong>web</strong> <span class="plan-note">new service</span>"#),
        "{html}"
    );
    assert!(html.contains("stack file valid · 1 service<"), "{html}");
}

#[test]
fn preview_of_a_new_stack_says_so_and_lists_no_redeploy() {
    let preview = ApplyPreview {
        stack: "notes".into(),
        current: None,
        warnings: Vec::new(),
        pins: Vec::new(),
        changed: Vec::new(),
        start_order: Vec::new(),
    };
    let html = PreviewView::new(preview, NOTES.into(), None)
        .render()
        .unwrap();
    assert!(
        html.contains("create stack <strong>notes</strong>"),
        "{html}"
    );
    assert!(
        html.contains(r#"redeploy <strong>no service</strong> <span class="plan-note">every service already runs this configuration</span>"#),
        "{html}"
    );
    // Every route of a new stack appears.
    assert!(
        html.contains(r#"<li class="plan-add"><span class="plan-mark" aria-hidden="true">+</span><span class="plan-text"><span class="visually-hidden">Adds: </span>route <strong>notes.example.com/ → api:8080</strong>"#),
        "{html}"
    );
    assert!(html.contains("a new stack"), "{html}");
    assert!(html.contains(">Apply</button>"), "{html}");
    // Nothing starts: no start order.
    assert!(!html.contains("Start order"), "{html}");
}

/// ADR-0056: the preview lists the start waves, then the stop-first replacements and the
/// services held back behind them.
#[tokio::test]
async fn preview_shows_the_start_order() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let name = |n: &str| n.parse::<crate::stack::Name>().unwrap();
    *app.stacks.changed.lock().unwrap() = ["app", "cache", "db", "web", "worker"]
        .map(str::to_owned)
        .to_vec();
    *app.stacks.start_order.lock().unwrap() = vec![
        Wave::Start(
            [(name("cache"), Vec::new()), (name("web"), Vec::new())]
                .into_iter()
                .collect(),
        ),
        Wave::Start(
            [(name("worker"), vec![name("cache"), name("web")])]
                .into_iter()
                .collect(),
        ),
        Wave::Replace(name("db")),
        Wave::HeldBack {
            service: name("app"),
            behind: vec![name("db")],
        },
    ];
    let html = body(
        app.send(post(
            "/apply/preview",
            &browser,
            FORM,
            encoded(&[("toml", NOTES)]),
        ))
        .await,
    )
    .await;
    assert!(
        html.contains(r#"<h2 id="order-title">Start order</h2><span class="card-note">each service starts once its own dependencies pass</span>"#),
        "{html}"
    );
    let order: Vec<&str> = html
        .split(r#"<ol class="plan start-order">"#)
        .nth(1)
        .and_then(|rest| rest.split("</ol>").next())
        .unwrap()
        .split("<li>")
        .skip(1)
        .collect();
    assert_eq!(order.len(), 4, "{html}");
    assert!(
        order[0].starts_with(r#"<span class="plan-mark" aria-hidden="true">1</span><span class="plan-text">start <strong>cache</strong>, <strong>web</strong></span></li>"#),
        "{}",
        order[0]
    );
    assert!(
        order[1].contains(r#"start <strong>worker</strong> <span class="plan-note">worker after cache, web</span>"#),
        "{}",
        order[1]
    );
    assert!(
        order[2].contains(r#"replace <strong>db</strong> <span class="plan-note">stop-first: down until the new one passes</span>"#),
        "{}",
        order[2]
    );
    assert!(
        order[3].contains(r#"<span class="plan-mark" aria-hidden="true">4</span>"#)
            && order[3].contains(r#"start <strong>app</strong> <span class="plan-note">held back until db is replaced and passes</span>"#),
        "{}",
        order[3]
    );
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
    assert!(
        html.contains(r#"href="/stacks/notes/environment""#),
        "{html}"
    );
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
    assert!(html.contains("re-pin <strong>storage</strong>"), "{html}");
    assert!(html.contains(&digest(0xbb)), "{html}");
    assert!(html.contains("redeploy <strong>storage</strong>"), "{html}");
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

#[tokio::test]
async fn the_overview_shows_kpis_responses_by_status_routes_and_volumes() {
    use crate::stats::CertExpiry;
    use crate::store::{DeploymentOutcome, Resolution, Timestamp};
    use crate::web::dashboard::tests::{MINUTE, bucket};

    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let toml = format!(
        "{NOTES}\n[[services.api.routes]]\nhost = \"notes.example.com\"\nprefix = \"/files\"\nstrip_prefix = true\nport = 9000\ntimeouts = {{ read = \"60s\" }}\n\n[services.api.volumes]\ndata = \"/data\"\n"
    );
    app.stacks.stacks.lock().unwrap()[0].revision = revision("notes", 1, &toml);
    // Two minutes back is flushed whatever the second.
    let minute = Resolution::Minute.bucket(Timestamp::now()) - MINUTE * 2;
    app.stats.traffic.lock().unwrap().push(bucket(
        minute,
        ("notes.example.com", "notes", "api"),
        &[(200, 2, 5), (503, 1, 5)],
    ));
    app.stats
        .deployments
        .lock()
        .unwrap()
        .push(DeploymentOutcome {
            stack: "notes".into(),
            state: DeploymentState::Failed,
            count: 2,
            mean: std::time::Duration::from_secs(5),
            max: std::time::Duration::from_secs(9),
        });
    app.stats.certificates.lock().unwrap().push(CertExpiry {
        host: "notes.example.com".into(),
        not_after: std::time::SystemTime::now() + std::time::Duration::from_secs(61 * 86_400 + 60),
    });

    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    // The tiles: 3 requests in the last 5 minutes, one 5xx, two failed deploys.
    assert!(
        html.contains(r#"<dt class="kpi-label">req/min · last hour</dt><dd class="kpi-value">0.6</dd><dd class="kpi-sub">peak 3, "#),
        "{html}"
    );
    assert!(
        html.contains(r#"<div class="kpi kpi-fail" title="Share of 5xx responses over the last hour"><dt class="kpi-label">5xx rate · last hour</dt><dd class="kpi-value">33.3 %</dd><dd class="kpi-sub">1 of 3</dd>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<dt class="kpi-label">deploys · 30 d</dt><dd class="kpi-value">0 live</dd><dd class="kpi-sub">2 failed</dd>"#),
        "{html}"
    );
    // The responses by status over the hour, with the legend's totals; no per-route figures.
    assert!(
        html.contains(r#"Responses by status <span class="card-count">· 1h</span></h2>"#),
        "{html}"
    );
    assert!(html.contains("2xx 2</li>"), "{html}");
    assert!(html.contains("5xx 1</li>"), "{html}");
    assert!(!html.contains("slow-route"), "{html}");
    // The routes card, with the certificate's days left and the route's settings.
    assert!(
        html.contains(r#"<a class="route-url" href="https://notes.example.com/files" target="_blank" rel="noopener noreferrer" title="Open https://notes.example.com/files in a new tab">https://notes.example.com/files<span aria-hidden="true"> ↗</span></a>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="route-target">api:9000</span>"#),
        "{html}"
    );
    assert!(html.contains(">TLS · 61 d left</span>"), "{html}");
    assert!(
        html.contains(r#"<span class="tag">strip prefix</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="tag">read timeout 1 min</span>"#),
        "{html}"
    );
    // The volumes card names where the active revision mounts each volume.
    assert!(
        html.contains(
            r#"<li><code>hoster-notes-data</code> <span class="muted">→ api:/data</span></li>"#
        ),
        "{html}"
    );
    assert!(!html.contains("style="), "{html}");
}

/// `NOTES` with config files (ADR-0057): two on `api`, one of them a secret that is set, and a
/// secret file on `storage` whose secret is not.
fn notes_with_files() -> String {
    format!(
        "{NOTES}\n[services.api.files]\n\
         \"/etc/notes/config.yml\" = {{ content = \"\"\"\nlisten: 8080\npassword: inline-hunter2\n\"\"\" }}\n\
         \"/run/secrets/db.json\" = {{ secret = \"notes/db\", mode = \"0440\", owner = \"0:65534\" }}\n\n\
         [services.storage.files]\n\"/etc/rustfs/license\" = {{ secret = \"notes/missing\" }}\n"
    )
}

/// ADR-0057 §10: the overview lists each file's path, mode, owner and size or secret name,
/// and never its content.
#[tokio::test]
async fn the_overview_lists_config_files_without_their_content() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    // No files, no card.
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(!html.contains(r#"id="files""#), "{html}");
    assert!(!html.contains("config file"), "{html}");

    app.stacks.stacks.lock().unwrap()[0].revision = revision("notes", 4, &notes_with_files());
    let response = app.get("/stacks/notes", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains(
            r#"<h2 id="files-title">Config files <span class="card-count">· 3</span></h2>"#
        ),
        "{html}"
    );
    // The service rows point at the card.
    assert!(
        html.contains(r##"<a class="service-deps" href="#files">2 config files</a>"##),
        "{html}"
    );
    assert!(
        html.contains(r##"<a class="service-deps" href="#files">1 config file</a>"##),
        "{html}"
    );
    let card = &html[html.find(r#"id="files""#).unwrap()..];
    let card = &card[..card.find("</section>").unwrap()];
    let cells: Vec<&str> = card
        .split("<tr>")
        .skip(2)
        .map(|row| row.split("</tr>").next().unwrap().trim())
        .collect();
    assert_eq!(cells.len(), 3, "{card}");
    // Inline: the defaults resolved, and the size.
    for part in [
        r#"<span class="mono">api</span>"#,
        r#"<code class="wrap">/etc/notes/config.yml</code>"#,
        r#"<td class="mono nowrap">0444</td>"#,
        r#"<td class="mono nowrap">0:0</td>"#,
        r#"<span class="tag tag-neutral">inline</span> <span class="muted nowrap">38 B</span>"#,
    ] {
        assert!(cells[0].contains(part), "{part} in {}", cells[0]);
    }
    // A secret that is set: its name, linked to where it is replaced.
    for part in [
        r#"<code class="wrap">/run/secrets/db.json</code>"#,
        r#"<td class="mono nowrap">0440</td>"#,
        r#"<td class="mono nowrap">0:65534</td>"#,
        r#"<a href="/stacks/notes/environment"><code class="env-secret">notes/db</code></a>"#,
    ] {
        assert!(cells[1].contains(part), "{part} in {}", cells[1]);
    }
    assert!(!cells[1].contains("not set"), "{}", cells[1]);
    // One that is not: marked like in the environment tab, owned by the container's user.
    for part in [
        r#"<span class="mono">storage</span>"#,
        r#"<td class="mono nowrap">0400</td>"#,
        r#"<td class="mono nowrap">container user</td>"#,
        r#"<code class="env-secret">notes/missing</code></a> <span class="pill pill-warn""#,
        "not set",
    ] {
        assert!(cells[2].contains(part), "{part} in {}", cells[2]);
    }
    // Neither inline nor secret content is on the page.
    assert!(!html.contains("inline-hunter2"), "{html}");
    assert!(!html.contains("listen: 8080"), "{html}");
}

/// The plan says which file makes a service redeploy.
#[tokio::test]
async fn the_preview_names_the_files_that_redeploy_a_service() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    app.stacks.stacks.lock().unwrap()[0].revision = revision("notes", 4, &notes_with_files());
    *app.stacks.changed.lock().unwrap() = vec!["api".into(), "storage".into()];

    let preview = |toml: String| {
        let (app, browser) = (&app, &browser);
        async move {
            let response = app
                .send(post(
                    "/apply/preview",
                    browser,
                    FORM,
                    encoded(&[("toml", &toml)]),
                ))
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            body(response).await
        }
    };
    let changed = notes_with_files()
        .replace("listen: 8080", "listen: 9090")
        .replace(
            "secret = \"notes/missing\"",
            "secret = \"notes/missing\", mode = \"0444\"",
        );
    let html = preview(changed).await;
    assert!(
        html.contains(
            r#"redeploy <strong>api</strong> <span class="plan-note">file /etc/notes/config.yml changed</span>"#
        ),
        "{html}"
    );
    assert!(
        html.contains(
            r#"redeploy <strong>storage</strong> <span class="plan-note">file /etc/rustfs/license changed</span>"#
        ),
        "{html}"
    );

    // Added and removed files are named too; a service redeployed for another reason says
    // nothing about files.
    let changed = notes_with_files()
        .replace("\"/run/secrets/db.json\"", "\"/run/secrets/database.json\"")
        .replace("RUSTFS_ADDRESS = \":9000\"", "RUSTFS_ADDRESS = \":9001\"");
    let html = preview(changed).await;
    assert!(
        html.contains(
            r#"redeploy <strong>api</strong> <span class="plan-note">files /run/secrets/db.json removed, /run/secrets/database.json added</span>"#
        ),
        "{html}"
    );
    assert!(
        html.contains(r#"redeploy <strong>storage</strong></span>"#),
        "{html}"
    );
    // The diff and the confirm form carry the file, like any stack file text, but the plan
    // never quotes content.
    let plan = &html[..html.find("</ol>").unwrap()];
    assert!(!plan.contains("inline-hunter2"), "{plan}");
}

/// ADR-0059: each service row shows its limits and runtime options from the committed
/// revision, sizes as `ByteSize` writes them; a crash loop names its last exit, OOM included.
#[tokio::test]
async fn the_overview_shows_limits_options_and_the_last_exit_of_a_crash_loop() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(!html.contains("runtime options"), "{html}");
    assert!(!html.contains("resource limits"), "{html}");

    let committed = NOTES.replace(
        "[services.storage]\n",
        "[services.storage]\nshm_size = \"256MiB\"\ninit = true\n\
         tmpfs = { \"/tmp\" = \"64MiB\", \"/run\" = \"1024KiB\" }\n\
         limits = { memory = \"512MiB\", pids = 256, cpus = 1.5 }\n",
    );
    {
        let mut stacks = app.stacks.stacks.lock().unwrap();
        let notes = &mut stacks[0];
        // The active revision raises the limit; the committed one is in effect.
        notes.revision = revision("notes", 3, &committed.replace("512MiB", "1GiB"));
        notes.committed = Some(revision("notes", 2, &committed));
        let api = &mut notes.services[0];
        api.restarts = RestartStats {
            restarts: 6,
            crash_looping: true,
            last_exit: Some(crate::orchestrator::ExitReason::OomKilled(
                crate::orchestrator::MemoryLimit(Some(crate::stack::ByteSize::mebibytes(64))),
            )),
        };
    }
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(
        html.contains(r#"<span class="service-deps" title="resource limits"><span class="visually-hidden">limits </span>memory 512MiB · pids 256 · cpus 1.5</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="service-deps" title="runtime options"><span class="visually-hidden">runtime options </span>shm_size 256MiB · tmpfs /run 1MiB, /tmp 64MiB · init</span>"#),
        "{html}"
    );
    assert_eq!(
        html.matches("resource limits").count(),
        1,
        "api sets none: {html}"
    );
    assert!(
        html.contains("restarted 6 times; last: OOM-killed (limit 64MiB)"),
        "{html}"
    );
}
