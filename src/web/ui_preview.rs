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
//! Live parts (the dashboard stream, deployment progress, log follow, the sidebar host card's
//! poll) cannot connect when the files are served statically, so their `hx-sse:connect` (or
//! poll) attributes are left out and their first content is rendered into the page instead.
//!
//! When a page gains a state the fixtures cannot show yet, extend [`Fixtures`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;

use super::assets::Assets;
use super::certificates::{AcmeDirectory, CertOverview, CertOwner, CertRow};
use super::charts::{LineChart, Percentiles, Sparkline, StackedBars, Timeline};
use super::settings::{EffectiveConfig, Settings};
use super::system::{Facts, System};
use super::ui::Tone;
use crate::acme::{Backoff, DnsMismatch, DnsVerdict, Validity};
use crate::config::ConfigSource;
use crate::health::{HealthStats, HealthStatus};
use crate::notify::{Letterhead, Mail, Notice};
use crate::orchestrator::{
    CertStatus, ExitReason, MemoryLimit, RestartStats, ServiceState, ServiceStatus, StackStatus,
};
use crate::registry::{RegistryCheck, RegistryCredential};
use crate::routing::{Owner, RouteEntry, RouteOwner, Target};
use crate::runtime::{
    ContainerRuntime, ContainerSpec, ContainerState, LogLine, LogStream, NetworkSpec, Operation,
    PortBinding, RuntimeError, ServiceGeneration, label,
};
use crate::stack::{ByteSize, RouteTimeouts};
use crate::stats::{CertExpiry, Histogram, Range, RequestOutcome, StatusClass, StatusCounts};
use crate::store::{
    ContentHash, Deployment, DeploymentEvent, DeploymentId, DeploymentOutcome, DeploymentSource,
    DeploymentState, DigestSet, Event, EventId, EventKind, Provenance, Resolution, RevisionId,
    Severity, StackRevision, Timestamp, TrafficBucket, TrafficCounts, TrafficSeries,
    WebhookRejection,
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
init = true
limits = { memory = "256MiB", pids = 256, cpus = 0.5 }
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
shm_size = "256MiB"
tmpfs = { "/tmp" = "64MiB", "/run/postgresql" = "1MiB" }
limits = { memory = "1GiB" }

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
    /// notes' updates: storage's tag moved two hours ago, a minor and a major were found a
    /// day ago, and api could not be checked.
    fn updates() -> crate::orchestrator::StackUpdates {
        use crate::orchestrator::UpdateOffer;
        use crate::store::UpdateKind;
        let offer = |kind, to: &str, digest: Option<String>, seen| UpdateOffer {
            kind,
            services: vec!["storage".into()],
            from: "1.0.0".into(),
            to: to.into(),
            digest: digest.map(|d| d.parse().unwrap()),
            first_seen: seen,
        };
        crate::orchestrator::StackUpdates {
            offers: vec![
                offer(
                    UpdateKind::Digest,
                    "1.0.0",
                    Some(digest(0xbb)),
                    ago(2 * HOUR),
                ),
                offer(UpdateKind::Minor, "1.1.0", None, ago(DAY)),
                offer(UpdateKind::Major, "2.0.0", None, ago(DAY)),
            ],
            errors: Vec::new(),
            checked_at: Some(ago(Duration::from_secs(20 * 60))),
        }
    }
}

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
                last_exit: crash_looping.then_some(ExitReason::OomKilled(MemoryLimit(Some(
                    ByteSize::mebibytes(256),
                )))),
            },
            health: HealthStats {
                status: health,
                last: (health != HealthStatus::Unknown)
                    .then(|| format!("GET /healthz of {name}: {}", Self::probe(health))),
                ..HealthStats::default()
            },
            waiting_for: Vec::new(),
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
    /// notes' stack file, with limits on api for the services table's meters.
    fn notes_toml() -> String {
        format!("{NOTES}\n[services.api.limits]\nmemory = \"512MiB\"\ncpus = 1.5\n")
    }

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
                committed: None,
                revision: Self::revision("notes", 1, &Self::notes_toml()),
                services: vec![notes_api, storage],
                active: None,
                queued: None,
                last: Some(notes_last),
                flag: None,
                rejections: vec![Self::rejection()],
            },
            StackStatus {
                name: "blog".into(),
                committed: None,
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
                committed: None,
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
                committed: None,
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
                committed: None,
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
    /// provenance), one in progress, one queued behind it, a rejected one, a superseded one, a
    /// no-op and an older one that went live with warnings.
    fn deployments() -> Vec<(Deployment, Vec<DeploymentEvent>)> {
        let event = |from, to, message: Option<&str>, at| DeploymentEvent {
            from,
            to,
            message: message.map(str::to_owned),
            at,
        };
        let mut live = Self::deployment("notes", 3, DeploymentState::Live, 2 * DAY);
        live.source = DeploymentSource::Webhook;
        live.provenance = Some(Self::provenance());
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
            7,
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
                DeploymentState::Starting,
                Some("starting storage"),
                r + s(8),
            ),
            event(
                Some(DeploymentState::Starting),
                DeploymentState::HealthChecking,
                None,
                r + s(9),
            ),
            event(
                Some(DeploymentState::HealthChecking),
                DeploymentState::HealthChecking,
                Some("checking storage with exec: rustfs health, declared in health.exec"),
                r + s(9),
            ),
            event(
                Some(DeploymentState::HealthChecking),
                DeploymentState::HealthChecking,
                Some(
                    r#"health probe exec: rustfs health: exit 1, output "error: connecting to 127.0.0.1:9000\nconnection refused""#,
                ),
                r + s(10),
            ),
            event(
                Some(DeploymentState::HealthChecking),
                DeploymentState::HealthChecking,
                Some("storage healthy after 4s 112ms; starting api"),
                r + s(13),
            ),
        ];
        let superseded = Self::deployment("notes", 6, DeploymentState::Superseded, 3 * HOUR);
        let mut cancelled = Self::deployment("notes", 5, DeploymentState::Failed, 5 * HOUR);
        cancelled.source = DeploymentSource::Manual;
        cancelled.error = Some(crate::orchestrator::CANCELLED.into());
        let mut noop = Self::deployment("notes", 2, DeploymentState::Noop, 3 * DAY);
        noop.source = DeploymentSource::Poller;
        // An older deployment that went live, with warnings, which the history offers to roll
        // back to.
        let mut first = Self::deployment("notes", 1, DeploymentState::LiveWithWarnings, 5 * DAY);
        first.source = DeploymentSource::Manual;
        first.target =
            DigestSet::from([("api".into(), digest(7)), ("storage".into(), digest(0xaa))]);
        first.finished_at = Some(first.created_at + Duration::from_secs(72));
        let w = first.created_at;
        let first_events = vec![
            event(None, DeploymentState::Received, None, w),
            event(
                Some(DeploymentState::Received),
                DeploymentState::Pulling,
                None,
                w + s(1),
            ),
            event(
                Some(DeploymentState::Pulling),
                DeploymentState::Switching,
                None,
                w + s(64),
            ),
            event(
                Some(DeploymentState::Switching),
                DeploymentState::LiveWithWarnings,
                Some("the old api container did not stop within 30 s and was killed"),
                w + s(72),
            ),
        ];
        // Waiting behind the running deployment.
        let mut queued =
            Self::deployment("notes", 8, DeploymentState::Queued, Duration::from_secs(5));
        queued.source = DeploymentSource::Poller;
        // Refused before anything ran.
        let mut rejected = Self::deployment(
            "notes",
            9,
            DeploymentState::Rejected,
            Duration::from_secs(2),
        );
        rejected.revision = None;
        rejected.generation = None;
        rejected.target = DigestSet::default();
        rejected.finished_at = Some(rejected.created_at);
        rejected.error = Some("stack file: service `api` has no image".into());
        vec![
            (first, first_events),
            (queued, Vec::new()),
            (rejected, Vec::new()),
            (noop, Vec::new()),
            (cancelled, Vec::new()),
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

    /// The event log: one event of each severity, with and without a page or a meta line.
    fn events() -> Vec<Event> {
        let event = |id: i64, age: Duration, kind, severity, message: &str| Event {
            id: EventId::from(id),
            at: ago(age),
            kind,
            severity,
            stack: None,
            subject: None,
            message: message.into(),
            meta: None,
        };
        vec![
            Event {
                stack: Some("notes".into()),
                subject: Some("/stacks/notes/deployments/41".into()),
                meta: Some("a91f3c2 · webhook · ghcr.io/acme/notes-api".into()),
                ..event(
                    6,
                    Duration::from_secs(40),
                    EventKind::DeployStarted,
                    Severity::Info,
                    "notes deploy started",
                )
            },
            Event {
                stack: Some("shop".into()),
                subject: Some("/stacks/shop".into()),
                meta: Some("generation 7 · attempt 4".into()),
                ..event(
                    5,
                    11 * Duration::from_secs(60),
                    EventKind::ContainerRestarted,
                    Severity::Warning,
                    "Restarted shop/web: OOM-killed (limit 256MiB)",
                )
            },
            Event {
                subject: Some("/certificates".into()),
                meta: Some("HTTP-01 · next try in 1 h".into()),
                ..event(
                    4,
                    2 * HOUR,
                    EventKind::CertificateFailed,
                    Severity::Error,
                    "Renewing the certificate for wiki.example.net failed",
                )
            },
            Event {
                stack: Some("notes".into()),
                subject: Some("/stacks/notes/deployments/40".into()),
                meta: Some("4e0b7d1 · webhook · 41 s".into()),
                ..event(
                    3,
                    5 * HOUR,
                    EventKind::DeploySucceeded,
                    Severity::Success,
                    "notes deployed",
                )
            },
            Event {
                subject: Some("/system".into()),
                meta: Some("v0.4.0".into()),
                ..event(
                    2,
                    DAY,
                    EventKind::HosterStarted,
                    Severity::Info,
                    "hoster started",
                )
            },
            event(
                1,
                3 * DAY,
                EventKind::SecretSet,
                Severity::Info,
                "octocat set the secret notes/db",
            ),
        ]
    }

    /// Valid, due for renewal, expiring, failing to renew a certificate that expires soon,
    /// failing behind a DNS mismatch while backing off without a certificate, and not attempted
    /// yet.
    fn certificates() -> CertOverview {
        let now = SystemTime::now();
        let status = |set: &dyn Fn(&mut CertStatus)| {
            let mut status = CertStatus::default();
            set(&mut status);
            status
        };
        let stack = |name: &str| CertOwner::Stacks(vec![name.to_owned()]);
        let issued = |host: &str, owner: CertOwner, left: Duration| CertRow {
            host: host.into(),
            issuer: Some("Let's Encrypt R11".into()),
            validity: Some(Validity {
                not_before: now + left - 90 * DAY,
                not_after: now + left,
            }),
            status: status(&|s| s.dns_checked = true),
            backoff: None,
            owner,
        };
        CertOverview {
            source: "the A/AAAA records of admin.example.com".into(),
            public: ["192.0.2.1".parse().unwrap(), "2001:db8::1".parse().unwrap()].into(),
            hosts: vec![
                issued("admin.example.com", CertOwner::Admin, 61 * DAY),
                CertRow {
                    status: status(&|s| {
                        s.error = Some(
                            "urn:ietf:params:acme:error:rateLimited: too many failed authorizations recently".into(),
                        );
                        s.retry_at = Some(now + 2 * HOUR);
                        s.dns_checked = true;
                    }),
                    backoff: Some(Backoff {
                        failures: 4,
                        retry_at: now + 2 * HOUR,
                    }),
                    ..issued("blog.example.com", stack("blog"), 3 * DAY)
                },
                issued("docs.example.com", stack("docs"), 21 * DAY),
                issued("acme.example.com", stack("docs"), 44 * DAY),
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
                    owner: stack("shop"),
                },
                CertRow {
                    host: "wiki.example.com".into(),
                    issuer: None,
                    validity: None,
                    status: CertStatus::default(),
                    backoff: None,
                    owner: stack("wiki"),
                },
                issued("www.blog.example.com", stack("blog"), 5 * DAY),
            ],
            directory: AcmeDirectory::LetsEncryptStaging,
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
            // A removed stack's route, counted before it went: the dashboard's note.
            (
                "forum.example.com",
                "/",
                "forum",
                "web",
                vec![(200, 12, 9), (502, 30, 2)],
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
        // hoster's own admin UI, and requests that matched no route: the dashboard's note.
        let admin = RouteEntry {
            owner: Owner::Reserved("admin UI"),
            ..Self::route("blog", "web", "/")
        };
        for i in 0..57 {
            app.traffic
                .route("admin.example.com", &admin)
                .finish(RequestOutcome {
                    status: Some(if i % 19 == 0 { 401 } else { 200 }),
                    latency: ms(3 + i % 5),
                    upstream_error: false,
                });
        }
        for i in 0..23 {
            app.traffic.unrouted().finish(RequestOutcome {
                status: Some(if i % 4 == 0 { 400 } else { 404 }),
                latency: ms(1),
                upstream_error: false,
            });
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
            outcome("forum", DeploymentState::Live, 2, 18, 20),
            outcome("forum", DeploymentState::Failed, 1, 9, 9),
            outcome("notes", DeploymentState::Live, 7, 14, 31),
            outcome("notes", DeploymentState::Failed, 1, 66, 66),
            outcome("blog", DeploymentState::Live, 12, 22, 58),
            outcome("blog", DeploymentState::Noop, 3, 1, 2),
            outcome("shop", DeploymentState::Live, 4, 41, 95),
            outcome("wiki", DeploymentState::Failed, 2, 61, 64),
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

    /// The stacks' containers: notes' api (with stdout and stderr lines) next to its draining
    /// previous generation and an internal storage, blog's web with an exited worker, and
    /// shop's storefront with the next generation starting, and one left behind by a removed
    /// `legacy` stack. Routed services publish a loopback port; each has been up for a while.
    async fn containers(app: &TestApp) -> String {
        let runtime = &app.runtime;
        let mut first = None;
        for (stack, service, generation, n, state, port, up) in [
            (
                "notes",
                "api",
                2,
                1,
                ContainerState::Running,
                Some(20_014),
                2 * HOUR + 240 * Duration::from_secs(1),
            ),
            (
                "notes",
                "api",
                1,
                9,
                ContainerState::Running,
                Some(20_003),
                3 * DAY,
            ),
            (
                "notes",
                "storage",
                2,
                0xaa,
                ContainerState::Running,
                None,
                12 * DAY + 6 * HOUR,
            ),
            (
                "blog",
                "worker",
                4,
                3,
                ContainerState::Exited(Some(1)),
                None,
                HOUR,
            ),
            (
                "blog",
                "web",
                4,
                2,
                ContainerState::Running,
                Some(20_007),
                5 * HOUR,
            ),
            (
                "shop",
                "storefront",
                7,
                4,
                ContainerState::Running,
                Some(20_011),
                DAY,
            ),
            (
                "shop",
                "storefront",
                8,
                5,
                ContainerState::Running,
                Some(20_015),
                Duration::from_secs(12),
            ),
            (
                "legacy",
                "web",
                3,
                6,
                ContainerState::Exited(Some(0)),
                None,
                40 * DAY,
            ),
        ] {
            let generation = ServiceGeneration {
                stack: stack.parse().unwrap(),
                service: service.parse().unwrap(),
                generation,
            };
            let image = format!("ghcr.io/acme/{service}@{}", digest(n));
            let mut spec =
                ContainerSpec::new(&generation, DeploymentId::from(1), image.parse().unwrap());
            spec.ports = port
                .map(|host_port| PortBinding {
                    container_port: 8080,
                    host_port,
                })
                .into_iter()
                .collect();
            runtime
                .ensure_network(&NetworkSpec::for_stack(&generation.stack))
                .await
                .unwrap();
            runtime.pull_image(&spec.image).await.unwrap();
            let id = runtime.create_container(&spec).await.unwrap();
            runtime.start(&id).await.unwrap();
            runtime.set_started_at(&id, SystemTime::now() - up);
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

/// Traffic history (ADR-0062) for the dashboard's range picker: 60 days of hour buckets, and
/// minute buckets for the last 12 hours (all the minute ranges and their previous periods
/// read), each route with its own daily wave. shop has a 5xx spike 10 hours ago, blog's 502s
/// follow its crash-looping worker, forum (removed) went quiet 3 days ago, wiki only came up 7
/// minutes ago (the stack pages' mostly empty hour), docs (not deployed) has none, and hoster's
/// own admin UI and unrouted requests feed the note under the stacks table.
impl Fixtures {
    /// A number from 0 to 1 that looks random but is the same on every run.
    fn noise(seed: u64) -> f64 {
        (seed.wrapping_mul(2_654_435_761) % 1_000) as f64 / 1_000.0
    }

    /// The bucket of `series` at `start` with `requests` requests, `latency` ms typical.
    fn bucket(
        start: Timestamp,
        (host, stack, service): (&str, &str, &str),
        requests: u64,
        latency: f64,
        errors: (u64, u64),
    ) -> TrafficBucket<Histogram> {
        let (client, server) = errors;
        let redirects = requests / 30;
        let ok = requests.saturating_sub(redirects + client + server);
        let mut histogram = Histogram::default();
        // A sample of the requests is enough for the percentiles; most are fast, a few slow.
        let samples = requests.min(120);
        for j in 0..samples {
            let tail = (j as f64 / samples as f64).powi(6);
            histogram.record(Duration::from_secs_f64(
                latency * (0.6 + 5.0 * tail) / 1_000.0,
            ));
        }
        TrafficBucket {
            start,
            series: TrafficSeries {
                host: host.into(),
                prefix: if host.is_empty() { "" } else { "/" }.into(),
                stack: stack.into(),
                service: service.into(),
            },
            counts: TrafficCounts {
                s2xx: ok,
                s3xx: redirects,
                s4xx: client,
                s5xx: server,
                upstream_errors: if stack == "blog" || stack == "forum" {
                    server
                } else {
                    0
                },
                ..TrafficCounts::default()
            },
            latency: histogram,
        }
    }

    fn history(app: &TestApp) {
        let now = Timestamp::now();
        let minute = Duration::from_secs(60);
        let newest = Resolution::Minute.bucket(now);
        let minutes_from = Resolution::Hour.bucket(now) - 12 * HOUR;
        let oldest = minutes_from - 60 * DAY;
        // (series, requests a minute at the daily peak, typical latency in ms, 4xx and 5xx
        // per mille)
        let routes = [
            (("notes.example.com", "notes", "api"), 40.0, 14.0, 12, 1),
            (("blog.example.com", "blog", "web"), 24.0, 38.0, 4, 6),
            (("www.blog.example.com", "blog", "web"), 5.0, 30.0, 2, 0),
            (("shop.example.com", "shop", "storefront"), 60.0, 85.0, 9, 2),
            (("forum.example.com", "forum", "web"), 8.0, 20.0, 5, 3),
            (("admin.example.com", "", "admin UI"), 2.0, 4.0, 15, 0),
            (("", "", ""), 0.8, 1.0, 1_000, 0),
        ];
        let mut rows = Vec::new();
        let mut push = |start: Timestamp, span: Duration| {
            let ago = Duration::from_millis((now.as_millis() - start.as_millis()).unsigned_abs());
            let day = DAY.as_secs_f64();
            let phase = (start.as_millis() as f64 / 1_000.0 % day) / day;
            let wave = 0.3 + 0.7 * (0.5 - 0.5 * (phase * std::f64::consts::TAU).cos());
            let minutes = span.as_secs_f64() / 60.0;
            for (n, &(series, peak, latency, client, server)) in routes.iter().enumerate() {
                // forum was removed 3 days ago.
                if series.1 == "forum" && ago < 3 * DAY {
                    continue;
                }
                let seed = start.as_millis().unsigned_abs() / 60_000 + n as u64 * 7_919;
                let rate = peak * wave * (0.8 + 0.4 * Self::noise(seed));
                let requests = (rate * minutes).round() as u64;
                if requests == 0 {
                    continue;
                }
                // shop's incident, 10 hours ago: an hour of 5xx and slow responses.
                let incident = series.1 == "shop" && ago >= 10 * HOUR && ago < 11 * HOUR;
                let server = if incident { 160 } else { server };
                let latency = if incident { latency * 4.0 } else { latency };
                let client = requests * client / 1_000;
                let server = if server == 0 {
                    0
                } else {
                    (requests * server).div_ceil(1_000).min(requests - client)
                };
                rows.push(Self::bucket(
                    start,
                    series,
                    requests,
                    latency,
                    (client, server),
                ));
            }
        };
        let mut start = oldest;
        while start < minutes_from {
            push(start, HOUR);
            start = start + HOUR;
        }
        while start < newest {
            push(start, minute);
            start = start + minute;
        }
        // wiki's first deploy went live 7 minutes ago, before the redeploy that failed: its
        // hour is mostly empty bars, a few of its minutes with 5xx.
        for back in 1..=7u32 {
            let requests = 6 + u64::from(back % 3) * 4;
            rows.push(Self::bucket(
                newest - minute * back,
                ("wiki.example.com", "wiki", "app"),
                requests,
                120.0,
                (1, u64::from(back % 2) * 3),
            ));
        }
        *app.stats.traffic.lock().unwrap() = rows;
    }
}

/// Resource usage (ADR-0066), an hour of samples: the sidebar's host card with CPU busy enough
/// to warn, memory a third used with a gap the sampler could not read, the disk nearly full; and
/// the containers' CPU and memory for the stack overviews. notes' api runs close to its memory
/// limit, its draining generation idles, storage (no limits) holds a large cache, blog's worker
/// stopped ten minutes ago and shop's new storefront has only just started. The empty app's
/// pages show the card before the first tick.
impl Fixtures {
    async fn resources(app: &TestApp) {
        const GIB: u64 = 1024 * 1024 * 1024;
        let n = crate::stats::ResourceUsage::HISTORY;
        let listed = app.runtime.list_managed().await.unwrap();
        for i in 0..n {
            let cpu = 30.0 + 50.0 * Self::noise(i as u64 + 7) + Self::wave(i, n, 0.0, 10.0) as f64;
            let gap = (200..212).contains(&i);
            let at = ago(Duration::from_secs(10 * (n - i) as u64));
            let containers = listed
                .iter()
                .enumerate()
                .map(|(k, info)| {
                    let figures = |i| Self::container_figures(info, k, i, n);
                    // A container's first reading has no CPU figure (ADR-0066 §7).
                    let earlier = i.checked_sub(1).and_then(figures);
                    let now = figures(i);
                    crate::stats::ContainerReading::from((
                        info,
                        crate::stats::ContainerSample {
                            at,
                            cpu: now.zip(earlier).map(|((cpu, _), _)| cpu),
                            memory: now.map(|(_, memory)| memory),
                        },
                    ))
                })
                .collect();
            app.resources.record(
                crate::stats::HostSample {
                    at,
                    // The first tick has no CPU figure (ADR-0066 §7).
                    cpu: (i > 0).then_some(cpu),
                    memory: (!gap).then_some(crate::stats::Usage {
                        used: 10 * GIB + Self::wave(i, n, 0.0, 2.0 * GIB as f64),
                        total: 32 * GIB,
                    }),
                    disk: Some(crate::stats::Usage {
                        used: 76 * GIB,
                        total: 80 * GIB,
                    }),
                },
                Some(containers),
            );
        }
    }

    /// The CPU (percent of one core) and memory of the `k`th listed container at sample `i` of
    /// `n`; `None` while it does not run.
    fn container_figures(
        info: &crate::runtime::ContainerInfo,
        k: usize,
        i: usize,
        n: usize,
    ) -> Option<(f64, u64)> {
        const MIB: u64 = 1024 * 1024;
        let label = |name| info.labels.get(name).map_or("", String::as_str);
        // A slow swing of its own per container, with a little noise on it.
        let swing = 0.5 - 0.5 * ((i as f64 / 23.0) + k as f64).cos();
        let jitter = 0.8 * swing + 0.2 * Self::noise((i * 31 + k * 977) as u64);
        let latest = label(label::GENERATION) == "8" || label(label::GENERATION) == "2";
        match (label(label::STACK), label(label::SERVICE)) {
            // Rising towards its 512 MiB limit: past 80 % by now.
            ("notes", "api") if latest => Some((
                20.0 + 40.0 * jitter,
                300 * MIB + (140 * MIB) * i as u64 / n as u64,
            )),
            ("notes", "api") => Some((0.5 + jitter, 180 * MIB)),
            ("notes", "storage") => Some((
                3.0 + 6.0 * jitter,
                1024 * MIB + Self::wave(i, n, 0.0, 400.0 * MIB as f64),
            )),
            ("blog", "worker") => (i < n - 60).then_some((80.0 + 60.0 * jitter, 90 * MIB)),
            ("shop", "storefront") if latest => (i >= n - 2).then_some((140.0, 210 * MIB)),
            _ if info.state.is_running() => Some((2.0 + 8.0 * jitter, 120 * MIB)),
            _ => None,
        }
    }
}

/// The traffic charts (ADR-0062 §9) on a page of their own, with fixture series: a day with a
/// 5xx spike, an hour, a week, an empty series and a single spike, and the sparklines in each
/// tone, beside the pages that draw them from history (the dashboard).
impl Fixtures {
    /// Requests in bar `i` of `n`: a daily wave between `low` and `high`.
    fn wave(i: usize, n: usize, low: f64, high: f64) -> u64 {
        let phase = i as f64 / n as f64 * std::f64::consts::TAU;
        (low + (high - low) * (0.5 - 0.5 * phase.cos())) as u64
    }

    /// Bars of `n` with a wave of traffic, a little 3xx and 4xx, and a 5xx spike at `spike`.
    fn status_bars(n: usize, high: f64, spike: Option<usize>) -> Vec<StatusCounts> {
        (0..n)
            .map(|i| {
                let total = Self::wave(i, n, high / 4.0, high);
                let errors = if Some(i) == spike {
                    total / 6
                } else {
                    total / 400
                };
                [
                    (
                        StatusClass::Success,
                        total - total / 40 - total / 60 - errors,
                    ),
                    (StatusClass::Redirection, total / 40),
                    (StatusClass::ClientError, total / 60),
                    (StatusClass::ServerError, errors),
                    (StatusClass::NoResponse, total / 2_000),
                ]
                .into_iter()
                .collect()
            })
            .collect()
    }

    /// p50 and p95 following the wave, slower at `spike`, with no samples in `gap`.
    fn latency_points(n: usize, spike: usize, gap: std::ops::Range<usize>) -> Vec<Percentiles> {
        (0..n)
            .map(|i| {
                if gap.contains(&i) {
                    return Percentiles::default();
                }
                let load = Self::wave(i, n, 0.0, 100.0);
                let p95 = if i == spike { 412 } else { 150 + load };
                Percentiles {
                    p50: Some(Duration::from_millis(30 + load / 5)),
                    p95: Some(Duration::from_millis(p95)),
                }
            })
            .collect()
    }

    fn charts() -> String {
        let card = |title: &str, note: &str, legend: &str, chart: &str| {
            format!(
                "<section class=\"card\"><div class=\"card-head\"><h2>{title} \
                 <span class=\"card-note\">· {note}</span></h2>{legend}</div>\
                 <div class=\"card-body\">{chart}</div></section>"
            )
        };
        let bars = |range: Range, bars: Vec<StatusCounts>, title: &str, note: &str| {
            let chart = StackedBars::new(Timeline::from(range), bars).of("all stacks");
            card(title, note, &chart.legend().to_string(), &chart.to_string())
        };
        let lines = |range: Range, points: Vec<Percentiles>, title: &str, note: &str| {
            let chart = LineChart::new(Timeline::from(range), points);
            card(title, note, &chart.legend().to_string(), &chart.to_string())
        };
        let mut single = vec![StatusCounts::default(); 24];
        single[14] = [
            (StatusClass::Success, 800),
            (StatusClass::ServerError, 1_203),
        ]
        .into_iter()
        .collect();
        let mut lone = vec![Percentiles::default(); 24];
        lone[14] = Percentiles {
            p50: Some(Duration::from_millis(48)),
            p95: Some(Duration::from_millis(612)),
        };

        let mut html = String::from("<div class=\"card-pair\">");
        html += &bars(
            Range::Day,
            Self::status_bars(24, 90_000.0, Some(14)),
            "Responses by status",
            "24h · a 5xx spike 10h ago",
        );
        html += &lines(
            Range::Day,
            Self::latency_points(24, 14, 5..7),
            "Response time",
            "24h · no samples 19h to 17h ago",
        );
        html += &bars(
            Range::Hour,
            Self::status_bars(60, 1_400.0, Some(50)),
            "Responses by status",
            "1h · 60 x 1 min",
        );
        html += &lines(
            Range::Hour,
            Self::latency_points(60, 50, 0..0),
            "Response time",
            "1h",
        );
        html += &bars(
            Range::Week,
            Self::status_bars(56, 300_000.0, None),
            "Responses by status",
            "7d · 56 x 3 h",
        );
        html += &lines(
            Range::ThirtyDays,
            Self::latency_points(30, 3, 20..30),
            "Response time",
            "30d · nothing in the last 10 days",
        );
        html += &bars(Range::Day, Vec::new(), "Responses by status", "24h · empty");
        html += &lines(Range::Day, Vec::new(), "Response time", "24h · empty");
        html += &bars(
            Range::Day,
            single,
            "Responses by status",
            "24h · a single spike",
        );
        html += &lines(Range::Day, lone, "Response time", "24h · a single bucket");
        // A few requests now and then, as on a quiet host: runs, lone samples and gaps.
        let mut sporadic = vec![Percentiles::default(); 60];
        for (minute, p50, p95) in [(29, 1, 20), (30, 1, 1), (31, 3, 3), (34, 5, 5), (51, 1, 1)] {
            sporadic[minute] = Percentiles {
                p50: Some(Duration::from_millis(p50)),
                p95: Some(Duration::from_millis(p95)),
            };
        }
        html += &lines(Range::Hour, sporadic, "Response time", "1h · sporadic load");
        html += "</div>";

        html += "<section class=\"card\"><div class=\"card-head\"><h2>Sparklines</h2></div>\
                 <div class=\"table-scroll\"><table class=\"list\"><thead><tr><th>Series</th>\
                 <th>Tone</th><th>Sparkline</th></tr></thead><tbody>";
        let wave: Vec<u64> = (0..24).map(|i| Self::wave(i, 24, 100.0, 900.0)).collect();
        let mut spike = vec![3; 60];
        spike[42] = 300;
        for (series, values, tone) in [
            ("a day of traffic", wave.clone(), Tone::Ok),
            ("deploying", wave.clone(), Tone::Progress),
            ("degraded", wave.clone(), Tone::Warn),
            ("failed", wave, Tone::Fail),
            ("a single spike", spike, Tone::Ok),
            ("all zero", vec![0; 24], Tone::Ok),
            ("stopped, no data", Vec::new(), Tone::Neutral),
        ] {
            let _ = write!(
                html,
                "<tr><td>{series}</td><td class=\"mono\">{tone}</td><td>{}</td></tr>",
                Sparkline::new(values, tone)
            );
        }
        html += "</tbody></table></div></section>";

        let mut page = String::from(
            "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <title>Charts · UI preview · hoster</title>",
        );
        let _ = writeln!(
            page,
            "<link rel=\"stylesheet\" href=\"{}\"></head><body><main class=\"content\">\
             <h1>Charts</h1>{html}</main></body></html>",
            Assets::get().stylesheet.url()
        );
        page
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
        std::fs::write(
            &path,
            Self::without_streams(html)
                .replace(Self::LIVE_CHECK, "")
                .replace(Self::HOST_POLL, ""),
        )
        .unwrap();
        self.index.push((file.to_owned(), label.to_owned()));
    }

    /// The stack editor's live check request (ADR-0060), left out like the streams: a static
    /// server cannot answer the POST. The states below show its panel filled in instead.
    const LIVE_CHECK: &str = " hx-post=\"/apply/check\"";

    /// The sidebar host card's poll, left out for the same reason.
    const HOST_POLL: &str = " hx-get=\"/shell/host\" hx-trigger=\"every 10s\"";

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

    /// `page` with the content of the `<div>` whose id is `id` replaced by `fragment`, as an
    /// htmx `innerHTML` swap does.
    fn replace_div(page: &str, id: &str, fragment: &str) -> String {
        let at = page
            .find(&format!("<div id=\"{id}\""))
            .unwrap_or_else(|| panic!("no div#{id} in the page"));
        let start = at + page[at..].find('>').unwrap() + 1;
        let mut depth = 1;
        let mut pos = start;
        while depth > 0 {
            let open = page[pos..].find("<div").map(|i| pos + i);
            let close = pos + page[pos..].find("</div>").expect("an unclosed div");
            match open {
                Some(open) if open < close => {
                    depth += 1;
                    pos = open + 4;
                }
                _ => {
                    depth -= 1;
                    pos = close + 6;
                }
            }
        }
        let end = pos - "</div>".len();
        format!("{}{fragment}{}", &page[..start], &page[end..])
    }

    /// [`replace_div`](Self::replace_div) for a response that ends in one
    /// `hx-swap-oob="innerHTML"` element: its content goes into the element with its id, the
    /// rest replaces the content of `div#id`.
    fn swap_with_oob(page: &str, id: &str, response: &str) -> String {
        let Some(at) = response
            .find(" hx-swap-oob=\"innerHTML\">")
            .and_then(|oob| response[..oob].rfind("<div id=\""))
        else {
            return Self::replace_div(page, id, response);
        };
        let (main, oob) = response.split_at(at);
        let oob_id = oob["<div id=\"".len()..].split('"').next().unwrap();
        let inner = &oob[oob.find('>').unwrap() + 1..oob.trim_end().len() - "</div>".len()];
        Self::inject(&Self::replace_div(page, id, main), oob_id, inner)
    }

    /// `page` with `text`, HTML-escaped, in the `<textarea>` whose id is `id`: what someone
    /// typed or uploaded into it before the htmx request.
    fn fill_textarea(page: &str, id: &str, text: &str) -> String {
        let at = page
            .find(&format!("<textarea id=\"{id}\""))
            .unwrap_or_else(|| panic!("no textarea#{id} in the page"));
        let start = at + page[at..].find('>').unwrap() + 1;
        let end = start + page[start..].find("</textarea>").unwrap();
        let escaped = text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        format!("{}{escaped}{}", &page[..start], &page[end..])
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
    *app.stacks.log.lock().unwrap() = Fixtures::events();
    // A volume of shop's that an earlier revision declared and the active one dropped.
    *app.stacks.earlier_volumes.lock().unwrap() = vec!["hoster-shop-redis".into()];
    // notes' storage: its tag moved, and a minor and a major are out (ADR-0061).
    app.stacks
        .updates
        .lock()
        .unwrap()
        .insert("notes".into(), Fixtures::updates());
    // notes' secrets: set some days ago, so their rows say when.
    for secret in app.stacks.secrets.lock().unwrap().iter_mut() {
        if secret.updated_at.is_some() {
            secret.updated_at = Some(ago(3 * DAY));
        }
    }
    *app.certificates.overview.lock().unwrap() = Some(Fixtures::certificates());
    app.certificates.dns.lock().unwrap().extend(
        [
            "admin.example.com",
            "blog.example.com",
            "www.blog.example.com",
            "docs.example.com",
            "acme.example.com",
            "wiki.example.com",
        ]
        .map(|host| (host.to_owned(), DnsVerdict::Matches))
        .into_iter()
        .chain([(
            "shop.example.com".to_owned(),
            DnsVerdict::Mismatch(DnsMismatch {
                host: "shop.example.com".into(),
                observed: vec!["203.0.113.9".parse().unwrap()],
                expected: vec!["192.0.2.1".parse().unwrap(), "2001:db8::1".parse().unwrap()],
            }),
        )]),
    );
    app.certificates
        .backing_off
        .lock()
        .unwrap()
        .insert("shop.example.com".into(), SystemTime::now() + HOUR);
    Fixtures::statistics(&app);
    Fixtures::history(&app);
    Fixtures::registries(&app);
    let container = Fixtures::containers(&app).await;
    Fixtures::resources(&app).await;
    let schema: i64 = crate::store::Store::expected_schema_version().into();
    let host = Host::new(&[("0.1.2", schema), ("0.1.3", schema)]).await;
    let mut releases = FakeReleases::default().publish_binary("0.2.0", script("0.2.0", schema));
    releases.releases[0].notes =
        "## Features\n- Stack pages share one header and tab bar\n- JetBrains Mono for machine-derived text\n\n## Fixes\n- <script> in notes is escaped".into();
    releases.releases[0].published = Some(SystemTime::now() - 2 * DAY);
    releases.releases[0].page = Some(
        "https://github.com/LHelge/hoster/releases/tag/v0.2.0"
            .parse()
            .unwrap(),
    );
    app.app.system = System::new(
        host.updater(releases, env!("CARGO_PKG_VERSION")),
        ShutdownRequest::default(),
        std::sync::Arc::new(Facts::sample()),
    )
    .started_at(SystemTime::now() - (9 * DAY + 4 * HOUR));
    let browser = app.sign_in().await;
    let session = Session { app: &app, browser };

    let logs = "/stacks/notes/logs?service=api&tail=100&since=&follow=on";
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
        ("/stacks/notes/environment", "Environment of notes"),
        ("/stacks/notes/deployments", "Deployments of notes"),
        ("/stacks/notes/deployments/3", "Deployment: live"),
        (
            "/stacks/notes/deployments/4",
            "Deployment: failed webhook deploy",
        ),
        ("/stacks/notes/deployments/7", "Deployment: in progress"),
        (
            "/stacks/notes/deployments/1",
            "Deployment: older live with warnings (roll back)",
        ),
        ("/stacks/notes/deployments/9", "Deployment: rejected"),
        ("/stacks/notes/deployments/8", "Deployment: queued"),
        ("/apply", "Apply a stack file"),
        ("/certificates", "Certificates"),
        ("/settings/general", "Settings: general"),
        (
            "/settings/notifications",
            "Settings: notifications, not configured",
        ),
        ("/settings/registries", "Settings: registries"),
        ("/settings/webhook", "Settings: deploy webhook"),
        ("/settings/proxy", "Settings: reverse proxy"),
        ("/settings/certificates", "Settings: certificates (ACME)"),
        ("/system", "System"),
        ("/activity", "Events"),
        (
            "/containers/hoster-legacy-web-g3/logs",
            "Container logs: leftover of a removed stack",
        ),
        ("/stacks/docs/logs", "Logs of docs: no container yet"),
    ] {
        let html = session.get(url, StatusCode::OK).await;
        preview.write(&Preview::file_of("", url), label, &html);
    }

    // The dashboard on its other ranges.
    for range in Range::ALL {
        let html = session
            .get(&format!("/?range={range}"), StatusCode::OK)
            .await;
        preview.write(
            &format!("states/dashboard-{range}.html"),
            &format!("Dashboard: the last {range}"),
            &html,
        );
    }

    // The stacks list filtered to one status.
    let html = session.get("/stacks?status=degraded", StatusCode::OK).await;
    preview.write(
        "states/stacks-degraded.html",
        "Stacks: degraded only",
        &html,
    );

    // The Events page filtered to one stack's warnings and worse.
    let html = session
        .get("/activity?stack=shop&severity=warning", StatusCode::OK)
        .await;
    preview.write(
        "states/events-shop-warnings.html",
        "Events: shop, warnings and worse",
        &html,
    );

    // Live parts, with their first content in place.
    let page = session.get(logs, StatusCode::OK).await;
    let lines = session
        .stream(&format!("/containers/{container}/logs/stream?tail=100"))
        .await;
    preview.write(
        &Preview::file_of("", logs),
        "Logs of notes: following, with a draining generation",
        // Still following: the closing notice has not arrived.
        &Session::inject(
            &page,
            "log",
            lines.split("<span class=\"log-notice\">").next().unwrap(),
        ),
    );
    // The draining container chosen, the log not followed, from a preset.
    let page = session
        .get(
            "/stacks/notes/logs?service=api&container=hoster-notes-api-g1&tail=100&since=24h",
            StatusCode::OK,
        )
        .await;
    preview.write(
        "states/stack-logs-draining.html",
        "Logs of notes: draining container, not following",
        &Session::inject(&page, "log", &lines),
    );

    // htmx results, each in the page that asks for it.
    let apply = session.get("/apply", StatusCode::OK).await;
    let edited = NOTES.replace("port = 8080", "port = 9090");
    for (file, label, toml, changed) in [
        (
            "states/apply-preview.html",
            "Apply: preview with diff",
            edited.as_str(),
            &["api"][..],
        ),
        (
            "states/apply-new-stack.html",
            "Apply: a new stack with a warning",
            "version = 1\nname = \"links\"\n\n[services.web]\nimage = \"docker.io/library/nginx:1.27\"\nvolumes = { data = \"/usr/share/nginx/html\" }\n\n[[services.web.routes]]\nhost = \"links.example.com\"\nport = 80\n\n[services.cache]\nimage = \"docker.io/library/redis:7\"\n",
            &["cache", "web"],
        ),
        (
            "states/apply-problems.html",
            "Apply: problems in the file",
            "version = 1\nname = \"Not Valid\"\n[services.web]\nimage = 3\n",
            &[],
        ),
    ] {
        *app.stacks.changed.lock().unwrap() = changed.iter().map(|&s| s.to_owned()).collect();
        let result = session.post("/apply/preview", &[("toml", toml)]).await;
        let live = session.post("/apply/check", &[("toml", toml)]).await;
        let typed = Session::inject(
            &Session::fill_textarea(&apply, "stack-toml", toml),
            "apply-check",
            &live,
        );
        preview.write(
            file,
            label,
            &Session::swap_with_oob(&typed, "apply-result", &result),
        );
    }
    // Editing the active revision: the file as applied, checked as the page loads; then the
    // preview once another revision became active meanwhile.
    *app.stacks.changed.lock().unwrap() = vec!["api".into()];
    let notes = app.stacks.current("notes").expect("the notes stack");
    let opened = notes.revision.revision.to_string();
    let edit = session.get("/apply?stack=notes", StatusCode::OK).await;
    let live = session
        .post("/apply/check", &[("toml", &notes.revision.toml)])
        .await;
    let edit = Session::inject(&edit, "apply-check", &live);
    preview.write("states/apply-edit.html", "Apply: editing notes", &edit);
    app.stacks
        .set_revision("notes", notes.revision.revision + 1);
    let stale = session
        .post(
            "/apply/preview",
            &[
                ("toml", &edited),
                ("base_stack", "notes"),
                ("base_revision", &opened),
            ],
        )
        .await;
    app.stacks.set_revision("notes", notes.revision.revision);
    preview.write(
        "states/apply-edit-stale.html",
        "Apply: editing notes, another revision became active",
        &Session::swap_with_oob(
            &Session::fill_textarea(&edit, "stack-toml", &edited),
            "apply-result",
            &stale,
        ),
    );
    // The diagnostics while typing, before Check: an unknown key, on its line.
    let typing = "version = 1\nname = \"links\"\n\n[services.web]\nimage = \"docker.io/library/nginx:1.27\"\nvolumes = { data = \"/usr/share/nginx/html\" }\n\n[[services.web.routes]]\nhost = \"links.example.com\"\nport = 80\nstrip = true\n";
    let live = session.post("/apply/check", &[("toml", typing)]).await;
    preview.write(
        "states/apply-typing.html",
        "Apply: diagnostics while typing",
        &Session::inject(
            &Session::fill_textarea(&apply, "stack-toml", typing),
            "apply-check",
            &live,
        ),
    );
    *app.stacks.changed.lock().unwrap() = vec!["api".into()];
    let stack = session.get("/stacks/notes", StatusCode::OK).await;
    let reresolve = session.post("/stacks/notes/reresolve/preview", &[]).await;
    preview.write(
        "states/stack-reresolve.html",
        "Stack: re-resolve preview",
        &Session::inject(&stack, "reresolve-result", &reresolve),
    );
    let update = session
        .post(
            "/stacks/notes/updates/preview",
            &[("updates", "storage:minor")],
        )
        .await;
    preview.write(
        "states/stack-update.html",
        "Stack: update preview",
        &Session::inject(&stack, "update-result", &update),
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
    app.runtime.fail_always(
        Operation::ListManaged,
        RuntimeError::Unreachable {
            socket: "/run/user/1000/podman/podman.sock".into(),
            message: "connection refused".into(),
        },
    );
    let html = session.get("/stacks/notes", StatusCode::OK).await;
    preview.write(
        "states/stack-podman-unreachable.html",
        "Stack: containers cannot be listed",
        &html,
    );
    app.runtime.clear_failures();
    let environment = session
        .get("/stacks/notes/environment", StatusCode::OK)
        .await;
    let review = session
        .post(
            "/stacks/notes/environment",
            &[("name", "db"), ("value", "not shown")],
        )
        .await;
    preview.write(
        "states/environment-save-review.html",
        "Environment: reviewing a secret save",
        &Session::inject(&environment, "secret-result", &review),
    );
    let delete = session
        .post("/stacks/notes/environment/delete", &[("name", "db")])
        .await;
    preview.write(
        "states/environment-delete-confirm.html",
        "Environment: deleting a referenced secret",
        &Session::inject(&environment, "delete-result", &delete),
    );
    let refused = session
        .post(
            "/stacks/notes/environment/delete",
            &[("name", "db"), ("confirm", "dbx")],
        )
        .await;
    preview.write(
        "states/environment-delete-refused.html",
        "Environment: deleting with the wrong name typed",
        &Session::inject(&environment, "delete-result", &refused),
    );
    let system = session.get("/system", StatusCode::OK).await;
    let check = session.post("/system/check", &[]).await;
    preview.write(
        "states/system-update-available.html",
        "System: update available",
        &Session::replace_div(&system, "update-check", &check),
    );
    // Applying it switches the fake layout and asks the (unconnected) shutdown to restart.
    let restarting = session
        .post("/system/update", &[("version", "v0.2.0")])
        .await;
    preview.write(
        "states/system-restarting.html",
        "System: update applied, restarting",
        &Session::replace_div(&system, "update-check", &restarting),
    );
    let blocked = session.post("/system/check", &[]).await;
    preview.write(
        "states/system-update-blocked.html",
        "System: update available but blocked (restarting)",
        &Session::replace_div(&system, "update-check", &blocked),
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
    let dns_all = session.post("/certificates/dns/all", &[]).await;
    preview.write(
        "states/certificates-dns-all.html",
        "Certificates: after Check all DNS",
        &Session::swap_with_oob(&certs, "cert-body", &dns_all),
    );
    let retry_failing = session.post("/certificates/retry/failing", &[]).await;
    preview.write(
        "states/certificates-retry-failing.html",
        "Certificates: Retry failing, one host backing off",
        &Session::inject(&certs, "cert-result", &retry_failing),
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
    let invalid = session
        .post(
            "/settings/notifications",
            &[
                ("provider", "resend"),
                ("api_key", ""),
                ("from", "hoster@"),
                ("recipient", ""),
                ("enabled", "on"),
            ],
        )
        .await;
    let notifications = session.get("/settings/notifications", StatusCode::OK).await;
    // The answer replaces the form's card (an `outerHTML` swap).
    let start = notifications.find("<section id=\"notify-form\"").unwrap();
    let end = start + notifications[start..].find("</section>").unwrap() + "</section>".len();
    preview.write(
        "states/notifications-invalid.html",
        "Notifications: field errors on save",
        &format!(
            "{}{invalid}{}",
            &notifications[..start],
            &notifications[end..]
        ),
    );
    session
        .post(
            "/settings/notifications",
            &[
                ("provider", "resend"),
                ("api_key", "re_preview_key"),
                ("from", "hoster@example.com"),
                ("recipient", "ops@example.com"),
                ("enabled", "on"),
            ],
        )
        .await;
    let test = session.post("/settings/notifications/test", &[]).await;
    let configured = session.get("/settings/notifications", StatusCode::OK).await;
    preview.write(
        "states/notifications-test.html",
        "Notifications: configured, test sent",
        &Session::inject(
            &configured,
            "notify-test-result",
            &test[..test.find("<div id=\"notify-status\"").unwrap_or(test.len())],
        ),
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
        (
            "/settings/general",
            "Empty: settings, public addresses not resolved",
        ),
        ("/settings/registries", "Empty: registries"),
        ("/system", "Empty: system (not an installed layout)"),
        ("/activity", "Empty: no events"),
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
        ("/stacks/notes/environment", "Empty: no stored secrets"),
    ] {
        let html = session.get(url, StatusCode::OK).await;
        preview.write(&Preview::file_of("empty", url), label, &html);
    }

    // Settings of an instance on staging with configured addresses and its own audience.
    let mut staging = TestApp::new().await;
    let config = EffectiveConfig::sample().with(|config| {
        config.acme.directory_url = "https://acme-staging-v02.api.letsencrypt.org/directory".into();
        config.acme.contact = None;
        config.acme.public_addresses = vec![
            "203.0.113.42".parse().unwrap(),
            "2001:db8::42".parse().unwrap(),
        ];
        config.web.dev_listen = Some("127.0.0.1:8080".parse().unwrap());
        config.podman_socket = Some("/run/user/1001/podman/podman.sock".into());
        config.source = ConfigSource::new(
            "/etc/hoster/hoster.toml",
            [
                "admin_github",
                "acme.directory_url",
                "acme.public_addresses",
            ],
        );
    });
    staging.app.settings = Settings::new(
        config,
        staging.certificates.clone(),
        staging.registries.clone(),
        staging.instance.clone(),
        staging.notifications.clone(),
    );
    let browser = staging.sign_in().await;
    let session = Session {
        app: &staging,
        browser,
    };
    for (slug, label) in [
        ("general", "Settings: configured addresses, dev listener"),
        ("certificates", "Settings: ACME staging, no contact"),
    ] {
        let html = session
            .get(&format!("/settings/{slug}"), StatusCode::OK)
            .await;
        preview.write(
            &format!("states/settings-{slug}-staging.html"),
            label,
            &html,
        );
    }

    // The traffic charts (ADR-0062 §9) with fixture series of every shape.
    preview.write(
        "components/charts.html",
        "Charts: stacked bars, response time lines, sparklines",
        &Fixtures::charts(),
    );

    // Notification mails (ADR-0058 §10): each variant's HTML part, with its plain-text part
    // beside it.
    let letterhead = Letterhead::new(
        "srv-01".parse().unwrap(),
        "https://hoster.example.com".parse().unwrap(),
    );
    let problem = crate::notify::mail_fixtures::problem();
    let recovery = crate::notify::mail_fixtures::recovery();
    let now = Timestamp::now();
    for (name, label, notice) in [
        ("problem", "Mail: a failed deploy", Notice::from(&problem)),
        (
            "recovery",
            "Mail: a crash loop left",
            Notice::from(&recovery),
        ),
        (
            "reminder",
            "Mail: still failing a day later",
            Notice::Reminder(&problem),
        ),
        (
            "summary",
            "Mail: the hourly cap's summary",
            Notice::Summary { more: 7, at: now },
        ),
        ("test", "Mail: the test message", Notice::Test { at: now }),
    ] {
        let mail = Mail::try_from((&letterhead, notice)).unwrap();
        preview.write(&format!("mail/{name}.html"), label, &mail.html);
        preview.write(
            &format!("mail/{name}.txt"),
            &format!("{label} (plain text)"),
            &mail.text,
        );
    }

    preview.finish();
}
