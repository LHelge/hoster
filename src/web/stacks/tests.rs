//! The stack pages against a fake orchestrator, through the whole app (sessions, CSRF), and
//! their templates rendered on their own.

use std::sync::Mutex;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt as _;

use super::*;
use crate::health::HealthStats;
use crate::orchestrator::{RestartStats, ServiceStatus};
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

fn digest(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

fn revision(stack: &str, number: u32, toml: &str) -> StackRevision {
    StackRevision {
        id: RevisionId::from(i64::from(number)),
        stack: stack.into(),
        revision: number,
        toml: toml.into(),
        hash: ContentHash::of(toml),
        created_at: Timestamp::now(),
    }
}

fn deployment(id: i64, state: DeploymentState) -> Deployment {
    Deployment {
        id: DeploymentId::from(id),
        stack: "notes".into(),
        revision: Some(RevisionId::from(1)),
        generation: Some(2),
        state,
        source: DeploymentSource::Apply,
        target: DigestSet::from([("api".into(), digest(1))]),
        error: None,
        created_at: Timestamp::now(),
        updated_at: Timestamp::now(),
        committed_at: None,
        finished_at: state.is_terminal().then(Timestamp::now),
    }
}

/// The `notes` stack, at revision 1 of [`NOTES`], api running generation 2.
fn notes_status() -> StackStatus {
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
    }
}

/// A call the stack pages made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Preview(String),
    Apply(String),
    Deploy(String, DigestSet),
    Remove(RemoveStack),
    ReresolvePreview(String),
    Reresolve(String, u32),
    /// A stored secret, with the value it was given (only the fake keeps it, to prove the
    /// pages never show it).
    SetSecret(String, String, String),
    DeleteSecret(String, String),
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
async fn stacks_list_shows_services_digests_revision_and_last_deploy() {
    let app = TestApp::new().await;
    let browser = app.sign_in().await;
    let response = app.get("/stacks", Some(&browser.cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body(response).await;
    assert!(
        html.contains(r#"<a href="/stacks" aria-current="page">Stacks</a>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/stacks/notes">notes</a>"#),
        "{html}"
    );
    let hash = ContentHash::of(NOTES).to_string();
    assert!(html.contains(&format!(r#"1 <code title="{hash}">{}</code>"#, &hash[..12])));
    assert!(html.contains(&format!(
        "<code>sha256:{}</code>, generation 2",
        "01".repeat(6)
    )));
    assert!(
        html.contains("<strong>storage</strong>: not deployed"),
        "{html}"
    );
    assert!(
        html.contains(r##"<a href="/stacks/notes/deployments/3">#3</a> Live"##),
        "{html}"
    );
    // The failed-deploy flag (ADR-0049), escaped.
    assert!(
        html.contains("deploy failed: &#60;pull&#62; denied"),
        "{html}"
    );
}

#[tokio::test]
async fn read_only_pages_need_a_session_but_no_javascript() {
    let app = TestApp::new().await;
    let response = app.get("/stacks", None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let browser = app.sign_in().await;
    let html = body(app.get("/stacks/notes", Some(&browser.cookie)).await).await;
    assert!(html.contains("<h1>Stack notes</h1>"), "{html}");
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
        html.contains(&format!("<code>{}</code>", digest(0xbb))),
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
        html.contains(r#"hx-get="/stacks/notes/deployments/7/status" hx-trigger="every 1s""#),
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
        !partial.contains("hx-trigger"),
        "a finished deployment stops polling"
    );
    assert!(partial.contains("<strong>Live</strong>"), "{partial}");

    for path in [
        "/stacks/other/deployments/7",
        "/stacks/notes/deployments/99",
        "/stacks/notes/deployments/x",
    ] {
        let response = app.get(path, Some(&browser.cookie)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
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
