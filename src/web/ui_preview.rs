//! The UI preview: every admin page rendered with fixture data into static files, for looking at
//! the design in a browser without running hoster (epic 8bf, ADR-0054). It is an ignored test,
//! so the normal test run skips it:
//!
//! ```sh
//! cargo test ui_preview -- --ignored
//! python3 -m http.server --bind 127.0.0.1 8765 -d target/ui-preview
//! # then open http://127.0.0.1:8765/pages.html
//! ```
//!
//! The app is the one the web tests use ([`TestApp`]: fake orchestrator, runtime, certificates,
//! registries and statistics) with a signed-in session. Each page is fetched through the router
//! and written at its own URL path (`/stacks/notes` → `stacks/notes/index.html`), so links
//! between pages work when the directory is served; the hashed assets (stylesheet, fonts, icon
//! sprite, htmx) go to `assets/` under the names the pages reference. Results of htmx actions
//! (an apply preview, an update check, …) are written as variants under `states/`, the page
//! with the partial put into its target element. `empty/` holds the pages of an app with no
//! stacks, deployments, certificates, registries or containers. `pages.html` links them all.
//!
//! Live parts (the dashboard stream, deployment progress, log follow) cannot connect when the
//! files are served statically, so their `hx-sse:connect` attributes are left out and their
//! first content is rendered into the page instead.
//!
//! When a page gains a state the fixtures cannot show yet, extend [`Fixtures`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;

use super::assets::Assets;
use super::certificates::{CertOverview, CertRow};
use super::system::System;
use crate::acme::{Backoff, DnsMismatch, DnsVerdict, Validity};
use crate::health::{HealthStats, HealthStatus};
use crate::orchestrator::{CertStatus, RestartStats, ServiceState, ServiceStatus, StackStatus};
use crate::registry::{RegistryCheck, RegistryCredential};
use crate::routing::{Owner, RouteEntry, RouteOwner, Target};
use crate::runtime::{
    ContainerRuntime, ContainerSpec, ContainerState, LogLine, LogStream, NetworkSpec,
    ServiceGeneration,
};
use crate::stack::RouteTimeouts;
use crate::stats::{CertExpiry, RequestOutcome, ServiceRestarts};
use crate::store::{
    ContentHash, Deployment, DeploymentEvent, DeploymentId, DeploymentOutcome, DeploymentSource,
    DeploymentState, DigestSet, Provenance, RevisionId, StackRevision, Timestamp, WebhookRejection,
};
use crate::supervisor::ShutdownRequest;
use crate::update::fake::{FakeReleases, Host, script};
use crate::web::auth::tests::{Browser, TestApp};
use crate::web::stacks::tests::{FORM, NOTES, encoded, post};

const DAY: Duration = Duration::from_secs(86_400);
const HOUR: Duration = Duration::from_secs(3_600);

const BLOG: &str = r#"version = 1
name = "blog"

[services.web]
image = "ghcr.io/acme/blog"

[[services.web.routes]]
host = "blog.example.com"
port = 8080

[[services.web.routes]]
host = "www.blog.example.com"
port = 8080

[services.worker]
image = "ghcr.io/acme/blog-worker"
"#;

const SHOP: &str = r#"version = 1
name = "shop"

[services.storefront]
image = "ghcr.io/acme/storefront"

[[services.storefront.routes]]
host = "shop.example.com"
port = 3000

[services.db]
image = "docker.io/library/postgres:17"

[services.db.volumes]
pgdata = "/var/lib/postgresql/data"
backups = "/backups"
"#;

const WIKI: &str = r#"version = 1
name = "wiki"

[services.app]
image = "ghcr.io/acme/wiki"

[[services.app.routes]]
host = "wiki.example.com"
port = 8080
"#;

const DOCS: &str = r#"version = 1
name = "docs"

[services.site]
image = "ghcr.io/acme/docs"

[[services.site.routes]]
host = "acme.example.com"
prefix = "/docs"
strip_prefix = true
port = 8080
timeouts = { read = "60s", idle = "120s" }

[[services.site.routes]]
host = "docs.example.com"
port = 8080
"#;

fn digest(n: u8) -> String {
    format!("sha256:{}", format!("{n:02x}").repeat(32))
}

fn ago(d: Duration) -> Timestamp {
    Timestamp::now() - d
}

/// Where the preview is written: `target/ui-preview` of this crate.
fn out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ui-preview")
}

/// The fixture data: one builder per fake, each covering the states its pages can show.
struct Fixtures;

impl Fixtures {
    fn revision(stack: &str, number: u32, toml: &str) -> StackRevision {
        StackRevision {
            id: RevisionId::from(i64::from(number)),
            stack: stack.into(),
            revision: number,
            toml: toml.into(),
            hash: ContentHash::of(toml),
            created_at: ago(3 * DAY),
        }
    }

    fn deployment(stack: &str, id: i64, state: DeploymentState, age: Duration) -> Deployment {
        let created_at = ago(age);
        Deployment {
            id: DeploymentId::from(id),
            stack: stack.into(),
            revision: Some(RevisionId::from(1)),
            generation: Some(2),
            state,
            source: DeploymentSource::Apply,
            target: DigestSet::from([("api".into(), digest(1))]),
            provenance: None,
            error: None,
            created_at,
            updated_at: created_at + Duration::from_secs(14),
            committed_at: None,
            finished_at: state
                .is_terminal()
                .then_some(created_at + Duration::from_secs(14)),
        }
    }

    fn service(
        name: &str,
        state: ServiceState,
        health: HealthStatus,
        restarts: u32,
        crash_looping: bool,
    ) -> ServiceStatus {
        ServiceStatus {
            name: name.into(),
            image: format!("ghcr.io/acme/{name}"),
            ci_built: true,
            state,
            restarts: RestartStats {
                restarts,
                crash_looping,
            },
            health: HealthStats {
                status: health,
                last: (health != HealthStatus::Unknown)
                    .then(|| format!("GET /healthz of {name}: {}", Self::probe(health))),
                ..HealthStats::default()
            },
        }
    }

    fn probe(health: HealthStatus) -> &'static str {
        match health {
            HealthStatus::Healthy => "200",
            _ => "503 Service Unavailable",
        }
    }

    fn running(generation: u32, n: u8) -> ServiceState {
        ServiceState::Running {
            generation,
            digest: digest(n),
        }
    }

    /// The OIDC claims of a webhook deploy of `notes` (ADR-0047).
    fn provenance() -> Provenance {
        Provenance {
            repository: "acme/notes".into(),
            sha: "4f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39".into(),
            git_ref: "refs/tags/v1.4.2".into(),
            run_id: "11223300".into(),
            actor: "octocat".into(),
            tag: Some("v1.4.2".into()),
        }
    }

    fn rejection() -> WebhookRejection {
        WebhookRejection {
            stack: "notes".into(),
            reason: "the [deploy] rule allows refs/tags/v*, not refs/heads/main".into(),
            repository: "acme/notes".into(),
            repository_id: "812345678".into(),
            git_ref: "refs/heads/main".into(),
            environment: None,
            workflow_ref: Some("acme/notes/.github/workflows/deploy.yml@refs/heads/main".into()),
            job_workflow_ref: None,
            actor: "octocat".into(),
            run_id: "11223344".into(),
            at: ago(2 * HOUR),
        }
    }

    /// `notes` healthy and live (with a refused webhook), `blog` with a crash-looping worker,
    /// `shop` mid-deployment with one queued, `wiki` whose latest deploy failed, `docs` waiting
    /// for its first digest (with a route under a prefix).
    fn stacks() -> Vec<StackStatus> {
        let mut notes_api =
            Self::service("api", Self::running(2, 1), HealthStatus::Healthy, 0, false);
        notes_api.image = "ghcr.io/acme/notes-api".into();
        let mut storage = Self::service(
            "storage",
            Self::running(2, 0xaa),
            HealthStatus::Unknown,
            1,
            false,
        );
        storage.image = "docker.io/rustfs/rustfs:1.0.0".into();
        storage.ci_built = false;
        let mut wiki_last = Self::deployment(
            "wiki",
            12,
            DeploymentState::Failed,
            40 * 60 * Duration::from_secs(1),
        );
        wiki_last.error = Some("health check of <app> timed out after 60 s".into());
        let mut notes_last = Self::deployment("notes", 3, DeploymentState::Live, 2 * DAY);
        notes_last.source = DeploymentSource::Webhook;
        notes_last.provenance = Some(Self::provenance());
        vec![
            StackStatus {
                name: "notes".into(),
                revision: Self::revision("notes", 1, NOTES),
                services: vec![notes_api, storage],
                active: None,
                queued: None,
                last: Some(notes_last),
                flag: None,
                rejections: vec![Self::rejection()],
            },
            StackStatus {
                name: "blog".into(),
                revision: Self::revision("blog", 4, BLOG),
                services: vec![
                    Self::service("web", Self::running(4, 2), HealthStatus::Healthy, 0, false),
                    Self::service(
                        "worker",
                        Self::running(4, 3),
                        HealthStatus::Unhealthy,
                        6,
                        true,
                    ),
                ],
                active: None,
                queued: None,
                last: Some(Self::deployment(
                    "blog",
                    9,
                    DeploymentState::LiveWithWarnings,
                    5 * HOUR,
                )),
                flag: None,
                rejections: Vec::new(),
            },
            StackStatus {
                name: "shop".into(),
                revision: Self::revision("shop", 2, SHOP),
                services: vec![
                    Self::service(
                        "storefront",
                        Self::running(7, 4),
                        HealthStatus::Healthy,
                        0,
                        false,
                    ),
                    Self::service(
                        "db",
                        ServiceState::NotDeployed,
                        HealthStatus::Unknown,
                        0,
                        false,
                    ),
                ],
                active: Some((DeploymentId::from(15), DeploymentState::Pulling)),
                queued: Some(DeploymentId::from(16)),
                last: Some(Self::deployment(
                    "shop",
                    16,
                    DeploymentState::Queued,
                    Duration::from_secs(5),
                )),
                flag: None,
                rejections: Vec::new(),
            },
            StackStatus {
                name: "wiki".into(),
                revision: Self::revision("wiki", 1, WIKI),
                services: vec![Self::service(
                    "app",
                    ServiceState::AwaitingFirstDigest,
                    HealthStatus::Unknown,
                    0,
                    false,
                )],
                active: None,
                queued: None,
                last: Some(wiki_last),
                flag: Some(
                    "revision 1: deploy failed: health check of <app> timed out after 60 s".into(),
                ),
                rejections: Vec::new(),
            },
            StackStatus {
                name: "docs".into(),
                revision: Self::revision("docs", 1, DOCS),
                services: vec![Self::service(
                    "site",
                    ServiceState::AwaitingFirstDigest,
                    HealthStatus::Unknown,
                    0,
                    false,
                )],
                active: None,
                queued: None,
                last: None,
                flag: None,
                rejections: Vec::new(),
            },
        ]
    }

    /// The history of `notes`: live (with transitions), a failed webhook deploy (with
    /// provenance), one in progress, a superseded one and a no-op.
    fn deployments() -> Vec<(Deployment, Vec<DeploymentEvent>)> {
        let event = |from, to, message: Option<&str>, at| DeploymentEvent {
            from,
            to,
            message: message.map(str::to_owned),
            at,
        };
        let live = Self::deployment("notes", 3, DeploymentState::Live, 2 * DAY);
        let t = live.created_at;
        let s = Duration::from_secs;
        let live_events = vec![
            event(None, DeploymentState::Received, None, t),
            event(
                Some(DeploymentState::Received),
                DeploymentState::Validating,
                None,
                t + s(1),
            ),
            event(
                Some(DeploymentState::Validating),
                DeploymentState::Pulling,
                None,
                t + s(1),
            ),
            event(
                Some(DeploymentState::Pulling),
                DeploymentState::Starting,
                Some("pulled ghcr.io/acme/notes-api"),
                t + s(6),
            ),
            event(
                Some(DeploymentState::Starting),
                DeploymentState::HealthChecking,
                None,
                t + s(7),
            ),
            event(
                Some(DeploymentState::HealthChecking),
                DeploymentState::Switching,
                Some("api healthy after 2 probes"),
                t + s(11),
            ),
            event(
                Some(DeploymentState::Switching),
                DeploymentState::Draining,
                None,
                t + s(11),
            ),
            event(
                Some(DeploymentState::Draining),
                DeploymentState::CleaningUp,
                None,
                t + s(13),
            ),
            event(
                Some(DeploymentState::CleaningUp),
                DeploymentState::Live,
                None,
                t + s(14),
            ),
        ];
        let mut failed = Self::deployment("notes", 4, DeploymentState::Failed, DAY);
        failed.source = DeploymentSource::Webhook;
        failed.error = Some("health check of <api> timed out".into());
        failed.target = DigestSet::from([("api".into(), digest(5))]);
        failed.provenance = Some(Self::provenance());
        let f = failed.created_at;
        let failed_events = vec![
            event(None, DeploymentState::Received, None, f),
            event(
                Some(DeploymentState::Received),
                DeploymentState::Pulling,
                None,
                f + s(1),
            ),
            event(
                Some(DeploymentState::Pulling),
                DeploymentState::Starting,
                None,
                f + s(4),
            ),
            event(
                Some(DeploymentState::Starting),
                DeploymentState::HealthChecking,
                None,
                f + s(5),
            ),
            event(
                Some(DeploymentState::HealthChecking),
                DeploymentState::Aborting,
                Some("health check of <api> timed out"),
                f + s(65),
            ),
            event(
                Some(DeploymentState::Aborting),
                DeploymentState::Failed,
                None,
                f + s(66),
            ),
        ];
        let running = Self::deployment(
            "notes",
            6,
            DeploymentState::HealthChecking,
            Duration::from_secs(20),
        );
        let r = running.created_at;
        let running_events = vec![
            event(None, DeploymentState::Received, None, r),
            event(
                Some(DeploymentState::Received),
                DeploymentState::Pulling,
                None,
                r + s(1),
            ),
            event(
                Some(DeploymentState::Pulling),
                DeploymentState::Starting,
                None,
                r + s(8),
            ),
            event(
                Some(DeploymentState::Starting),
                DeploymentState::HealthChecking,
                None,
                r + s(9),
            ),
        ];
        let superseded = Self::deployment("notes", 5, DeploymentState::Superseded, 3 * HOUR);
        let mut noop = Self::deployment("notes", 2, DeploymentState::Noop, 3 * DAY);
        noop.source = DeploymentSource::Poller;
        vec![
            (noop, Vec::new()),
            (live, live_events),
            (failed, failed_events),
            (superseded, Vec::new()),
            (running, running_events),
            // shop's last good deployment, which its Redeploy queues behind the running one.
            (
                Self::deployment("shop", 14, DeploymentState::Live, DAY),
                Vec::new(),
            ),
        ]
    }

    /// Valid, expiring, failing behind a DNS mismatch while backing off, and not attempted yet.
    fn certificates() -> CertOverview {
        let now = SystemTime::now();
        let status = |set: &dyn Fn(&mut CertStatus)| {
            let mut status = CertStatus::default();
            set(&mut status);
            status
        };
        let issued = |host: &str, left: Duration| CertRow {
            host: host.into(),
            issuer: Some("Let's Encrypt R11".into()),
            validity: Some(Validity {
                not_before: now + left - 90 * DAY,
                not_after: now + left,
            }),
            status: status(&|s| s.dns_checked = true),
            backoff: None,
        };
        CertOverview {
            source: "the A/AAAA records of admin.example.com".into(),
            public: ["192.0.2.1".parse().unwrap(), "2001:db8::1".parse().unwrap()].into(),
            hosts: vec![
                issued("admin.example.com", 61 * DAY),
                issued("blog.example.com", 5 * DAY),
                CertRow {
                    host: "shop.example.com".into(),
                    issuer: None,
                    validity: None,
                    status: status(&|s| {
                        s.error = Some(
                            "urn:ietf:params:acme:error:connection: <connection refused>".into(),
                        );
                        s.retry_at = Some(now + HOUR);
                        s.dns_mismatch = Some(DnsMismatch {
                            host: "shop.example.com".into(),
                            observed: vec!["203.0.113.9".parse().unwrap()],
                            expected: vec![
                                "192.0.2.1".parse().unwrap(),
                                "2001:db8::1".parse().unwrap(),
                            ],
                        });
                        s.dns_checked = true;
                    }),
                    backoff: Some(Backoff {
                        failures: 3,
                        retry_at: now + HOUR,
                    }),
                },
                CertRow {
                    host: "wiki.example.com".into(),
                    issuer: None,
                    validity: None,
                    status: CertStatus::default(),
                    backoff: None,
                },
                issued("www.blog.example.com", 5 * DAY),
            ],
        }
    }

    fn route(stack: &str, service: &str, prefix: &str) -> RouteEntry {
        RouteEntry {
            owner: Owner::Route(RouteOwner {
                stack: stack.parse().unwrap(),
                service: service.parse().unwrap(),
            }),
            prefix: prefix.parse().unwrap(),
            prefix_str: prefix.into(),
            strip_prefix: false,
            target: Target::Unavailable {
                retry_after: Duration::from_secs(1),
            },
            timeouts: RouteTimeouts::default(),
        }
    }

    /// Requests of every status class, slow and fast, plus the dashboard's other statistics.
    fn statistics(app: &TestApp) {
        let ms = Duration::from_millis;
        for (host, prefix, stack, service, requests) in [
            (
                "notes.example.com",
                "/",
                "notes",
                "api",
                vec![(200, 12, 180), (304, 3, 12), (404, 8, 6)],
            ),
            (
                "blog.example.com",
                "/",
                "blog",
                "web",
                vec![(200, 40, 640), (301, 9, 30), (502, 900, 4)],
            ),
            (
                "www.blog.example.com",
                "/",
                "blog",
                "web",
                vec![(200, 15, 60), (301, 2, 41)],
            ),
            (
                "shop.example.com",
                "/",
                "shop",
                "storefront",
                vec![(200, 85, 90), (500, 1200, 2), (429, 2, 11)],
            ),
            (
                "acme.example.com",
                "/docs",
                "docs",
                "site",
                vec![(200, 4, 30)],
            ),
        ] {
            let route = Self::route(stack, service, prefix);
            for (status, latency, count) in requests {
                for i in 0..count {
                    app.traffic.route(host, &route).finish(RequestOutcome {
                        status: Some(status),
                        latency: ms(latency + (i % 7) * latency / 3),
                        upstream_error: status == 502,
                    });
                }
            }
        }
        // One request still in flight; its guard is never dropped, so it stays counted.
        std::mem::forget(
            app.traffic
                .route("blog.example.com", &Self::route("blog", "web", "/")),
        );
        let outcome = |stack: &str, state, count, mean, max| DeploymentOutcome {
            stack: stack.into(),
            state,
            count,
            mean: Duration::from_secs(mean),
            max: Duration::from_secs(max),
        };
        app.stats.deployments.lock().unwrap().extend([
            outcome("notes", DeploymentState::Live, 7, 14, 31),
            outcome("notes", DeploymentState::Failed, 1, 66, 66),
            outcome("blog", DeploymentState::Live, 12, 22, 58),
            outcome("blog", DeploymentState::Noop, 3, 1, 2),
            outcome("shop", DeploymentState::Live, 4, 41, 95),
            outcome("wiki", DeploymentState::Failed, 2, 61, 64),
        ]);
        let restarts = |stack: &str, service: &str, restarts, crash_looping| ServiceRestarts {
            stack: stack.into(),
            service: service.into(),
            generation: 4,
            restarts,
            crash_looping,
        };
        app.stats.services.lock().unwrap().extend([
            restarts("blog", "worker", 6, true),
            restarts("notes", "storage", 1, false),
        ]);
        let now = SystemTime::now();
        app.stats.certificates.lock().unwrap().extend(
            [
                ("admin.example.com", 61 * DAY),
                ("blog.example.com", 5 * DAY),
                ("www.blog.example.com", 5 * DAY),
                ("notes.example.com", 44 * DAY),
                ("docs.example.com", 60 * DAY),
            ]
            .map(|(host, left)| CertExpiry {
                host: host.into(),
                not_after: now + left,
            }),
        );
    }

    fn registries(app: &TestApp) {
        app.registries.stored.lock().unwrap().extend([
            (
                "ghcr.io".parse().unwrap(),
                RegistryCredential::new("acme-bot", "never-shown"),
            ),
            (
                "registry.example.com".parse().unwrap(),
                RegistryCredential::new("deploy", "never-shown"),
            ),
        ]);
        app.registries
            .checks
            .lock()
            .unwrap()
            .push(("ghcr.io".into(), Ok(RegistryCheck::Authenticated)));
    }

    /// A running api container with stdout and stderr lines, and an exited worker.
    async fn containers(app: &TestApp) -> String {
        let runtime = &app.runtime;
        let mut first = None;
        for (stack, service, generation, n, state) in [
            ("notes", "api", 2, 1, ContainerState::Running),
            ("blog", "worker", 4, 3, ContainerState::Exited(Some(1))),
            ("blog", "web", 4, 2, ContainerState::Running),
        ] {
            let generation = ServiceGeneration {
                stack: stack.parse().unwrap(),
                service: service.parse().unwrap(),
                generation,
            };
            let image = format!("ghcr.io/acme/{service}@{}", digest(n));
            let spec =
                ContainerSpec::new(&generation, DeploymentId::from(1), image.parse().unwrap());
            runtime
                .ensure_network(&NetworkSpec::for_stack(&generation.stack))
                .await
                .unwrap();
            runtime.pull_image(&spec.image).await.unwrap();
            let id = runtime.create_container(&spec).await.unwrap();
            runtime.start(&id).await.unwrap();
            runtime.set_state(&id, state);
            if first.is_none() {
                first = Some(spec.name.clone());
                let t = |s: &str| Some(humantime::parse_rfc3339(s).unwrap());
                for (stream, time, text) in [
                    (
                        LogStream::Stdout,
                        "2026-11-08T12:00:00Z",
                        "notes-api 1.4.1 starting",
                    ),
                    (
                        LogStream::Stdout,
                        "2026-11-08T12:00:00Z",
                        "listening on 0.0.0.0:8080",
                    ),
                    (
                        LogStream::Stdout,
                        "2026-11-08T12:00:03Z",
                        "GET /healthz 200 0.4ms",
                    ),
                    (
                        LogStream::Stderr,
                        "2026-11-08T12:00:09Z",
                        "warning: <no config> using defaults",
                    ),
                    (
                        LogStream::Stdout,
                        "2026-11-08T12:01:12Z",
                        "POST /api/notes 201 12.8ms user=4711 note=\"a very long line that wraps inside the log box instead of widening the page at narrow widths\"",
                    ),
                    (
                        LogStream::Stderr,
                        "2026-11-08T12:02:40Z",
                        "error: upstream db timed out after 5s, retrying",
                    ),
                ] {
                    runtime.push_log(
                        &id,
                        LogLine {
                            stream,
                            time: t(time),
                            text: text.into(),
                        },
                    );
                }
            }
        }
        first.unwrap()
    }
}

/// The preview being written: its pages and the list `pages.html` links.
struct Preview {
    dir: PathBuf,
    index: Vec<(String, String)>,
}

impl Preview {
    fn new(dir: PathBuf) -> Self {
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        for asset in Assets::get().all() {
            std::fs::write(dir.join("assets").join(asset.hashed_name()), asset.bytes()).unwrap();
        }
        Self {
            dir,
            index: Vec::new(),
        }
    }

    /// Writes `html` at `file` (relative to the preview), and lists it as `label`.
    fn write(&mut self, file: &str, label: &str, html: &str) {
        let path = self.dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, Self::without_streams(html)).unwrap();
        self.index.push((file.to_owned(), label.to_owned()));
    }

    /// `html` without its `hx-sse:connect` attributes: a static server has no streams, and
    /// htmx would swap the server's 404 page into the element.
    fn without_streams(html: &str) -> String {
        const ATTRIBUTE: &str = " hx-sse:connect=\"";
        let mut out = String::with_capacity(html.len());
        let mut rest = html;
        while let Some(at) = rest.find(ATTRIBUTE) {
            out.push_str(&rest[..at]);
            let value = &rest[at + ATTRIBUTE.len()..];
            rest = &value[value.find('"').map_or(value.len(), |end| end + 1)..];
        }
        out.push_str(rest);
        out
    }

    /// The file a page at `url` is written to, so that serving the directory resolves the
    /// page's links.
    fn file_of(prefix: &str, url: &str) -> String {
        let path = url.split('?').next().unwrap().trim_matches('/');
        match (prefix, path) {
            ("", "") => "index.html".to_owned(),
            ("", path) => format!("{path}/index.html"),
            (prefix, "") => format!("{prefix}/index.html"),
            (prefix, path) => format!("{prefix}/{path}/index.html"),
        }
    }

    fn finish(self) {
        let mut html = String::from(
            "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <title>UI preview · hoster</title>",
        );
        let _ = write!(
            html,
            "<link rel=\"stylesheet\" href=\"{}\"></head><body><main><h1>UI preview</h1><ul>",
            Assets::get().stylesheet.url()
        );
        for (file, label) in &self.index {
            let _ = write!(
                html,
                "<li><a href=\"/{file}\">{label}</a> <code>{file}</code></li>"
            );
        }
        html.push_str("</ul></main></body></html>\n");
        std::fs::write(self.dir.join("pages.html"), html).unwrap();
        println!(
            "UI preview: {} pages in {}",
            self.index.len(),
            self.dir.display()
        );
    }
}

/// A signed-in browser on a [`TestApp`].
struct Session<'a> {
    app: &'a TestApp,
    browser: Browser,
}

impl Session<'_> {
    async fn text(response: axum::response::Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// `GET url` as the signed-in admin; asserts the expected status.
    async fn get(&self, url: &str, expect: StatusCode) -> String {
        let response = self.app.get(url, Some(&self.browser.cookie)).await;
        assert_eq!(response.status(), expect, "GET {url}");
        Self::text(response).await
    }

    /// An htmx form post's answer.
    async fn post(&self, url: &str, fields: &[(&str, &str)]) -> String {
        let response = self
            .app
            .send(post(url, &self.browser, FORM, encoded(fields)))
            .await;
        Self::text(response).await
    }

    /// The data of a finished SSE stream, every event's lines joined.
    async fn stream(&self, url: &str) -> String {
        let request = Request::get(url)
            .header("cookie", &self.browser.cookie)
            .body(Body::empty())
            .unwrap();
        let response = self.app.send(request).await;
        assert_eq!(response.status(), StatusCode::OK, "GET {url}");
        let mut body = response.into_body();
        let mut text = String::new();
        let read = async {
            while let Some(Ok(frame)) = body.frame().await {
                if let Ok(data) = frame.into_data() {
                    text.push_str(std::str::from_utf8(&data).unwrap());
                }
                if text.contains("event: end") {
                    break;
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), read).await;
        text.lines()
            .filter_map(|line| line.strip_prefix("data: ").or(line.strip_prefix("data:")))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `page` with `fragment` as the content of the element whose id is `id`.
    fn inject(page: &str, id: &str, fragment: &str) -> String {
        let at = page
            .find(&format!("id=\"{id}\""))
            .unwrap_or_else(|| panic!("no #{id} in the page"));
        let end = at + page[at..].find('>').unwrap() + 1;
        format!("{}{fragment}{}", &page[..end], &page[end..])
    }
}

#[tokio::test]
#[ignore = "writes the UI preview to target/ui-preview; run with --ignored"]
async fn ui_preview() {
    let mut preview = Preview::new(out_dir());

    // The full app: every state the fixtures have.
    let mut app = TestApp::new().await;
    *app.stacks.stacks.lock().unwrap() = Fixtures::stacks();
    *app.stacks.deployments.lock().unwrap() = Fixtures::deployments();
    // A volume of shop's that an earlier revision declared and the active one dropped.
    *app.stacks.earlier_volumes.lock().unwrap() = vec!["hoster-shop-redis".into()];
    *app.certificates.overview.lock().unwrap() = Some(Fixtures::certificates());
    app.certificates
        .dns
        .lock()
        .unwrap()
        .insert("admin.example.com".into(), DnsVerdict::Matches);
    app.certificates
        .backing_off
        .lock()
        .unwrap()
        .insert("shop.example.com".into(), SystemTime::now() + HOUR);
    Fixtures::statistics(&app);
    Fixtures::registries(&app);
    let container = Fixtures::containers(&app).await;
    let schema: i64 = crate::store::Store::expected_schema_version().into();
    let host = Host::new(&[("0.1.2", schema), ("0.1.3", schema)]).await;
    let mut releases = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema));
    releases.releases[0].notes =
        "## Features\n- Stack pages share one header and tab bar\n- JetBrains Mono for machine-derived text\n\n## Fixes\n- <script> in notes is escaped".into();
    app.app.system = System::new(
        host.updater(releases, env!("CARGO_PKG_VERSION")),
        ShutdownRequest::default(),
    );
    app.app.shell = app.app.shell.clone().with_host("srv-01");
    let browser = app.sign_in().await;
    let session = Session { app: &app, browser };

    let logs = format!("/containers/{container}/logs?tail=100");
    for (url, label) in [
        ("/", "Dashboard"),
        ("/stacks", "Stacks"),
        ("/stacks/notes", "Stack: notes (healthy, refused webhook)"),
        ("/stacks/blog", "Stack: blog (crash looping)"),
        ("/stacks/shop", "Stack: shop (deploying, queued)"),
        ("/stacks/wiki", "Stack: wiki (deploy failed)"),
        (
            "/stacks/docs",
            "Stack: docs (not deployed, stripped prefix with timeouts)",
        ),
        ("/stacks/notes/secrets", "Secrets of notes"),
        ("/stacks/notes/deployments", "Deployments of notes"),
        ("/stacks/notes/deployments/3", "Deployment: live"),
        (
            "/stacks/notes/deployments/4",
            "Deployment: failed webhook deploy",
        ),
        ("/stacks/notes/deployments/6", "Deployment: in progress"),
        ("/apply", "Apply a stack file"),
        ("/certificates", "Certificates"),
        ("/settings/registries", "Settings: registries"),
        ("/system", "System"),
        ("/containers", "Containers"),
    ] {
        let html = session.get(url, StatusCode::OK).await;
        preview.write(&Preview::file_of("", url), label, &html);
    }

    // The stacks list filtered to one status.
    let html = session.get("/stacks?status=degraded", StatusCode::OK).await;
    preview.write(
        "states/stacks-degraded.html",
        "Stacks: degraded only",
        &html,
    );

    // Live parts, with their first content in place.
    let page = session.get(&logs, StatusCode::OK).await;
    let lines = session
        .stream(&format!("/containers/{container}/logs/stream?tail=100"))
        .await;
    preview.write(
        &Preview::file_of("", &logs),
        "Container logs",
        &Session::inject(&page, "log", &lines),
    );

    // htmx results, each in the page that asks for it.
    let apply = session.get("/apply", StatusCode::OK).await;
    let edited = NOTES.replace("port = 8080", "port = 9090");
    for (file, label, toml) in [
        (
            "states/apply-preview.html",
            "Apply: preview with diff",
            edited.as_str(),
        ),
        (
            "states/apply-problems.html",
            "Apply: problems in the file",
            "version = 1\nname = \"Not Valid\"\n[services.web]\nimage = 3\n",
        ),
    ] {
        let result = session.post("/apply/preview", &[("toml", toml)]).await;
        preview.write(
            file,
            label,
            &Session::inject(&apply, "apply-result", &result),
        );
    }
    let stack = session.get("/stacks/notes", StatusCode::OK).await;
    let reresolve = session.post("/stacks/notes/reresolve/preview", &[]).await;
    preview.write(
        "states/stack-reresolve.html",
        "Stack: re-resolve preview",
        &Session::inject(&stack, "reresolve-result", &reresolve),
    );
    let deploy = session
        .post("/stacks/notes/deploy", &[("digest.api", &digest(0x42))])
        .await;
    preview.write(
        "states/stack-deployed.html",
        "Stack: manual deploy result",
        &Session::inject(&stack, "deploy-result", &deploy),
    );
    let remove = session
        .post("/stacks/notes/remove", &[("confirm", "note")])
        .await;
    preview.write(
        "states/stack-remove-refused.html",
        "Stack: removal without the name typed",
        &Session::inject(&stack, "remove-result", &remove),
    );
    let system = session.get("/system", StatusCode::OK).await;
    let check = session.post("/system/check", &[]).await;
    preview.write(
        "states/system-update-available.html",
        "System: update available",
        &Session::inject(&system, "update-check", &check),
    );
    let certs = session.get("/certificates", StatusCode::OK).await;
    let retry = session
        .post("/certificates/retry", &[("host", "shop.example.com")])
        .await;
    preview.write(
        "states/certificates-retry-refused.html",
        "Certificates: retry refused while backing off",
        &Session::inject(&certs, "cert-result", &retry),
    );
    let registries = session.get("/settings/registries", StatusCode::OK).await;
    let test = session
        .post("/settings/registries/test", &[("host", "ghcr.io")])
        .await;
    preview.write(
        "states/registries-test.html",
        "Registries: test result",
        &Session::inject(&registries, "test-result", &test),
    );

    // The error page and the public login page. (Unknown URLs answer a plain-text 404.)
    let html = session.get("/stacks/missing", StatusCode::NOT_FOUND).await;
    preview.write("states/unknown-stack.html", "Error: unknown stack", &html);
    let response = app.get("/auth/login", None).await;
    let html = Session::text(response).await;
    preview.write("auth/login/index.html", "Login", &html);
    let response = app
        .get("/auth/github/callback?error=access_denied", None)
        .await;
    let html = Session::text(response).await;
    preview.write("states/login-denied.html", "Login: denied by GitHub", &html);

    // An app with nothing in it.
    let empty = TestApp::new().await;
    empty.stacks.stacks.lock().unwrap().clear();
    let browser = empty.sign_in().await;
    let session = Session {
        app: &empty,
        browser,
    };
    for (url, label) in [
        ("/", "Empty: dashboard"),
        ("/stacks", "Empty: stacks"),
        ("/certificates", "Empty: certificates"),
        ("/settings/registries", "Empty: registries"),
        ("/containers", "Empty: containers"),
        ("/system", "Empty: system (not an installed layout)"),
    ] {
        let html = session.get(url, StatusCode::OK).await;
        preview.write(&Preview::file_of("empty", url), label, &html);
    }
    // A stack with no deployments and no secrets.
    let fresh = TestApp::new().await;
    {
        let mut stacks = fresh.stacks.stacks.lock().unwrap();
        stacks[0].last = None;
        stacks[0].flag = None;
    }
    fresh.stacks.secrets.lock().unwrap().clear();
    let browser = fresh.sign_in().await;
    let session = Session {
        app: &fresh,
        browser,
    };
    for (url, label) in [
        ("/stacks/notes", "Empty: a stack never deployed"),
        ("/stacks/notes/deployments", "Empty: no deployments"),
        ("/stacks/notes/secrets", "Empty: no secrets"),
    ] {
        let html = session.get(url, StatusCode::OK).await;
        preview.write(&Preview::file_of("empty", url), label, &html);
    }

    preview.finish();
}
