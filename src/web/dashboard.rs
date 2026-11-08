//! The dashboard at `/` (spec §5.4, ADR-0021, ADR-0045): every stack and service with its
//! running digest, health, restarts, last deployment and the expiry of the certificates its
//! routes are served with, under the warnings that are v1's only alerting surface, followed by
//! hoster's in-process statistics.
//!
//! The page renders server-side and works without JavaScript. The overview (warnings and
//! stacks) is one partial, [`Overview`]: the page holds it in a live region that follows
//! `GET /events` over SSE. The stream sends the partial again whenever the orchestrator reports
//! a change on `ui_events` (a health change, a certificate issued or failed, a deployment
//! moving) or the receiver lagged, and at least every [`OverviewEvents::REFRESH`] for what no
//! event reports (restart counts, a removed stack, certificates nearing expiry with time). It
//! ends with an `end` event when the session ends (after replacing the overview with a notice,
//! so a page signed out elsewhere does not keep showing a stale status as if it were live) or
//! the orchestrator is gone, which closes the connection. The statistics below are a snapshot
//! of when the page was loaded.
//!
//! A warning is raised for a live service whose health probe fails, a service in a crash loop
//! (by the health monitor or the reconciler, ADR-0028), a served certificate that expires within
//! [`CertState::WARN_WITHIN`], and a stack whose latest deployment failed or was rejected.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use askama::Template;
use axum::Extension;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream, StreamExt};
use tokio::sync::broadcast;

use super::auth::{Auth, Session};
use super::pages::{Layout, Page, Section, Shell};
use super::stacks::{DeploymentView, ShortDigest, StackView, Stacks, When};
use super::ui::{CertState, Pill, Tone};
use crate::health::HealthStatus;
use crate::orchestrator::{ServiceState, ServiceStatus, StackStatus, UiEvent};
use crate::routing::Owner;
use crate::stack::Stack;
use crate::stats::{CertExpiry, Counts, Elapsed, ServiceRestarts, Snapshot, Stats, StatusClass};
use crate::store::{Deployment, DeploymentState};

/// The dashboard: the live overview, then the in-process statistics of ADR-0045.
#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct Dashboard {
    layout: Layout,
    overview: Overview,
    stats: StatsView,
}

impl Dashboard {
    /// The URL of the overview's live stream.
    pub const EVENTS: &str = "/events";

    /// The `GET /` handler.
    pub async fn show(
        shell: Shell,
        Extension(stats): Extension<Stats>,
        Extension(stacks): Extension<Stacks>,
    ) -> Page<Dashboard> {
        let snapshot = stats.snapshot().await;
        let overview = Overview::gather(&stacks, &snapshot.certificates).await;
        Page(Dashboard {
            layout: shell.layout(Section::Dashboard),
            overview,
            stats: StatsView::from(snapshot),
        })
    }

    /// `GET /events`: the overview partial as `text/event-stream`, sent at once and again on
    /// every change the orchestrator reports, until the session ends.
    pub async fn events(
        State(auth): State<Arc<Auth>>,
        Extension(session): Extension<Session>,
        Extension(stats): Extension<Stats>,
        Extension(stacks): Extension<Stacks>,
    ) -> Response {
        // Subscribed before the first read, so no change between the two is missed.
        let events = stacks.subscribe();
        let signed_out = async move {
            if let Err(err) = auth
                .session_ended(&session, OverviewEvents::SESSION_CHECK)
                .await
            {
                tracing::error!(error = %err, "checking the session of the dashboard stream failed");
            }
        };
        let live = OverviewEvents { stacks, stats };
        Sse::new(live.stream(events, signed_out))
            .keep_alive(KeepAlive::new().interval(OverviewEvents::KEEP_ALIVE))
            .into_response()
    }

    pub fn events_url(&self) -> &'static str {
        Self::EVENTS
    }

    /// The page with no stacks and no statistics, for the layout tests.
    #[cfg(test)]
    pub(super) fn empty(layout: Layout) -> Self {
        Dashboard {
            layout,
            overview: Overview::default(),
            stats: StatsView::default(),
        }
    }
}

/// The overview as SSE events.
struct OverviewEvents {
    stacks: Stacks,
    stats: Stats,
}

impl OverviewEvents {
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
    /// The longest an open overview goes without being rendered again, for the changes no
    /// [`UiEvent`] reports.
    const REFRESH: Duration = Duration::from_secs(60);
    /// What replaces the overview once the session has ended.
    const SIGNED_OUT: &str = r#"<p class="flash flash-error">Signed out: <a href="/">sign in again</a> to see the live status.</p>"#;

    /// The overview now, then once per burst of changes (or [`Self::REFRESH`]), until the
    /// orchestrator is gone or `cut` resolves (the session ended, which sends
    /// [`Self::SIGNED_OUT`] first); then `end`.
    fn stream(
        self,
        events: broadcast::Receiver<UiEvent>,
        cut: impl Future<Output = ()> + Send + 'static,
    ) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
        /// One item of the merged stream: an update, or the end (with a last notice).
        enum Item {
            Update(Event),
            Stop(Option<Event>),
        }
        let updates = stream::unfold(Some((self, events, true)), |state| async move {
            let (this, mut events, first) = state?;
            if !first && !Self::changed(&mut events).await {
                return None;
            }
            let event = this.render().await;
            Some((event, Some((this, events, false))))
        });
        let merged = stream::select(
            updates
                .map(Item::Update)
                .chain(stream::once(std::future::ready(Item::Stop(None)))),
            stream::once(cut).map(|()| Item::Stop(Some(Event::default().data(Self::SIGNED_OUT)))),
        )
        .boxed();
        // Unfolded rather than taken while, so the stream ends right at its stop instead of
        // waiting on whichever side has not finished.
        stream::unfold(Some(merged), |merged| async move {
            let mut merged = merged?;
            match merged.next().await? {
                Item::Update(event) => Some((event, Some(merged))),
                Item::Stop(Some(notice)) => Some((notice, None)),
                Item::Stop(None) => None,
            }
        })
        .chain(stream::once(std::future::ready(
            Event::default().event(Self::END).data(""),
        )))
        .map(Ok)
    }

    /// Waits for the next change (or at most [`Self::REFRESH`]), then takes every change
    /// already queued with it, so a burst (a deployment's transitions, a round of health
    /// probes) renders once. `false` once the orchestrator is gone: nothing changes any more.
    async fn changed(events: &mut broadcast::Receiver<UiEvent>) -> bool {
        match tokio::time::timeout(Self::REFRESH, events.recv()).await {
            // Nothing reported a change, but time passed (certificate expiry, restarts).
            Err(_) => return true,
            Ok(Ok(_) | Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) => return false,
        }
        loop {
            match events.try_recv() {
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(broadcast::error::TryRecvError::Empty) => return true,
                Err(broadcast::error::TryRecvError::Closed) => return false,
            }
        }
    }

    async fn render(&self) -> Event {
        let overview = Overview::gather(&self.stacks, &self.stats.certificates()).await;
        let html = overview.render().unwrap_or_else(|err| {
            tracing::error!(error = %err, "rendering the dashboard overview failed");
            String::new()
        });
        Event::default().data(html)
    }
}

/// The live part of the dashboard: the warnings and every stack with its services.
#[derive(Debug, Default, Template)]
#[template(path = "dashboard_overview.html")]
pub struct Overview {
    warnings: Vec<Warning>,
    stacks: Vec<StackRow>,
    /// Why the stacks cannot be shown, when the orchestrator failed.
    problem: Option<String>,
}

impl Overview {
    /// The stacks as the orchestrator reports them now, with the served `certificates`.
    async fn gather(stacks: &Stacks, certificates: &[CertExpiry]) -> Self {
        match stacks.status().await {
            Ok(status) => Self::new(&status, certificates, SystemTime::now()),
            Err(err) => {
                tracing::warn!(error = %err, "cannot read the stacks for the dashboard");
                Overview {
                    problem: Some(format!("Cannot show the stacks: {err}.")),
                    ..Self::new(&[], certificates, SystemTime::now())
                }
            }
        }
    }

    /// The overview of `stacks` and the served `certificates`, as of `now`.
    fn new(stacks: &[StackStatus], certificates: &[CertExpiry], now: SystemTime) -> Self {
        let expiry: BTreeMap<&str, &CertExpiry> = certificates
            .iter()
            .map(|cert| (cert.host.as_str(), cert))
            .collect();
        let mut warnings = Vec::new();
        let mut routed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let rows = stacks
            .iter()
            .map(|status| {
                let hosts = StackRow::hosts(status);
                for (service, hosts) in &hosts {
                    for host in hosts {
                        routed
                            .entry(host.clone())
                            .or_default()
                            .push(format!("{service} ({})", status.name));
                    }
                }
                let row = StackRow::new(status, &hosts, &expiry, now);
                warnings.extend(row.warnings(status));
                row
            })
            .collect();
        for cert in certificates {
            let left = cert.remaining(now);
            if CertState::from(left).is_flagged() {
                // A host no stack routes is hoster's own, a removed stack's, or one whose
                // stacks cannot be read: who uses it is not known here, so nobody is named.
                let detail = match routed.get(&cert.host) {
                    Some(users) => {
                        format!("{}, used by {}", CertCell::left(left), users.join(", "))
                    }
                    None => CertCell::left(left),
                };
                warnings.push(Warning {
                    kind: WarningKind::CertExpiring,
                    subject: cert.host.clone(),
                    detail,
                    href: "/certificates".to_owned(),
                });
            }
        }
        Overview {
            warnings,
            stacks: rows,
            problem: None,
        }
    }
}

/// Something on the dashboard that needs the operator's attention (ADR-0045).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Warning {
    kind: WarningKind,
    /// What it is about: `api (notes)`, `notes`, a host.
    subject: String,
    detail: String,
    /// Where to look into it.
    href: String,
}

impl Warning {
    /// The kind, as a pill in its tone.
    fn pill(&self) -> Pill {
        Tone::from(self.kind).pill(self.kind)
    }
}

/// The kinds of [`Warning`]; the `Display` is the pill text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WarningKind {
    /// The health probe of a live service fails (ADR-0038).
    Unhealthy,
    /// A service keeps crashing (ADR-0028).
    CrashLooping,
    /// A served certificate expires within [`CertState::WARN_WITHIN`].
    CertExpiring,
    /// A stack's latest deployment failed or was rejected.
    DeployFailed,
}

impl fmt::Display for WarningKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unhealthy => "failing health check",
            Self::CrashLooping => "crash looping",
            Self::CertExpiring => "certificate expiring",
            Self::DeployFailed => "deploy failed",
        })
    }
}

/// The same tone the condition has in its own cell: a failing probe and a crash loop as in
/// [`HealthStatus`], a failed deploy as [`DeploymentState::Failed`].
impl From<WarningKind> for Tone {
    fn from(kind: WarningKind) -> Self {
        match kind {
            WarningKind::Unhealthy | WarningKind::CrashLooping | WarningKind::DeployFailed => {
                Tone::Fail
            }
            WarningKind::CertExpiring => Tone::Warn,
        }
    }
}

/// One stack in the overview.
#[derive(Debug)]
struct StackRow {
    name: String,
    url: String,
    history_url: String,
    /// "revision N: deploy failed: …" (ADR-0049).
    flag: Option<String>,
    deployment: Option<DeployCell>,
    queued: Option<String>,
    services: Vec<ServiceRow>,
}

impl StackRow {
    /// The hosts each service of the stack's active revision routes. Empty when the revision
    /// does not parse (it did when it was applied, so only a newer hoster could refuse it).
    fn hosts(status: &StackStatus) -> BTreeMap<String, Vec<String>> {
        let Ok(stack) = status.revision.toml.parse::<Stack>() else {
            return BTreeMap::new();
        };
        stack
            .services
            .iter()
            .map(|(name, service)| {
                let mut hosts: Vec<String> =
                    service.routes.iter().map(|r| r.host.to_string()).collect();
                hosts.sort();
                hosts.dedup();
                (name.to_string(), hosts)
            })
            .collect()
    }

    fn new(
        status: &StackStatus,
        hosts: &BTreeMap<String, Vec<String>>,
        expiry: &BTreeMap<&str, &CertExpiry>,
        now: SystemTime,
    ) -> Self {
        let url = StackView::url(&status.name);
        let deployment = match (status.active, &status.last) {
            (Some((id, state)), _) => Some(DeployCell {
                id: id.to_string(),
                url: DeploymentView::url(&status.name, id),
                state,
                detail: "running".to_owned(),
            }),
            (None, Some(last)) => Some(DeployCell::from(last)),
            (None, None) => None,
        };
        StackRow {
            history_url: format!("{url}/deployments"),
            url,
            name: status.name.clone(),
            flag: status.flag.clone(),
            deployment,
            queued: status.queued.map(|id| id.to_string()),
            services: status
                .services
                .iter()
                .map(|service| {
                    let certs = hosts
                        .get(&service.name)
                        .into_iter()
                        .flatten()
                        .map(|host| CertCell::new(host, expiry.get(host.as_str()).copied(), now))
                        .collect();
                    ServiceRow::new(service, certs)
                })
                .collect(),
        }
    }

    /// How many table rows the stack takes: one per service, or one saying it has none.
    fn rows(&self) -> usize {
        self.services.len().max(1)
    }

    /// The health, crash-loop and deploy warnings of this stack (certificates are warned
    /// about once per host, by [`Overview::new`]).
    fn warnings(&self, status: &StackStatus) -> Vec<Warning> {
        let mut warnings = Vec::new();
        for service in &self.services {
            let subject = format!("{} ({})", service.name, self.name);
            let kind = if service.crash_looping {
                WarningKind::CrashLooping
            } else if service.running && service.health == HealthStatus::Unhealthy {
                WarningKind::Unhealthy
            } else {
                continue;
            };
            warnings.push(Warning {
                kind,
                subject,
                detail: service.health_detail.clone().unwrap_or_default(),
                href: self.url.clone(),
            });
        }
        if let Some(deployment) = self.deployment.as_ref().filter(|d| d.failed()) {
            let error = status
                .last
                .as_ref()
                .and_then(|last| last.error.clone())
                .unwrap_or_default();
            warnings.push(Warning {
                kind: WarningKind::DeployFailed,
                subject: self.name.clone(),
                detail: format!("deployment {} {}: {error}", deployment.id, deployment.state)
                    .trim_end_matches([':', ' '])
                    .to_owned(),
                href: deployment.url.clone(),
            });
        }
        warnings
    }
}

/// A stack's running or latest deployment.
#[derive(Debug)]
struct DeployCell {
    id: String,
    url: String,
    state: DeploymentState,
    /// "running", or "webhook, <when>".
    detail: String,
}

impl DeployCell {
    /// It ended `Failed` or `Rejected`.
    fn failed(&self) -> bool {
        Tone::from(self.state) == Tone::Fail
    }

    fn pill(&self) -> Pill {
        Pill::from(self.state)
    }
}

impl From<&Deployment> for DeployCell {
    fn from(deployment: &Deployment) -> Self {
        DeployCell {
            id: deployment.id.to_string(),
            url: DeploymentView::url(&deployment.stack, deployment.id),
            state: deployment.state,
            detail: if deployment.state.is_terminal() {
                format!("{}, {}", deployment.source, When(deployment.updated_at))
            } else {
                "running".to_owned()
            },
        }
    }
}

/// One service in the overview.
#[derive(Debug)]
struct ServiceRow {
    name: String,
    /// The running digest, shortened and in full.
    digest: Option<(String, String)>,
    /// What it runs, in words: `generation 3`, `not deployed`.
    state: String,
    /// Whether a generation runs, so health and restarts mean something.
    running: bool,
    health: HealthStatus,
    /// What the last probe (or the reconciler) said.
    health_detail: Option<String>,
    restarts: u32,
    /// By the health monitor's verdict or the reconciler's (ADR-0028).
    crash_looping: bool,
    /// The certificates of the hosts it routes.
    certs: Vec<CertCell>,
}

impl ServiceRow {
    fn new(service: &ServiceStatus, certs: Vec<CertCell>) -> Self {
        let (state, digest, running) = match &service.state {
            ServiceState::Running { generation, digest } => (
                format!("generation {generation}"),
                Some((ShortDigest(digest).to_string(), digest.clone())),
                true,
            ),
            ServiceState::AwaitingFirstDigest => {
                ("awaiting its first digest".to_owned(), None, false)
            }
            ServiceState::NotDeployed => ("not deployed".to_owned(), None, false),
        };
        ServiceRow {
            name: service.name.clone(),
            digest,
            state,
            running,
            health: service.health.status,
            health_detail: service.health.last.clone(),
            restarts: service.restarts.restarts,
            crash_looping: service.is_crash_looping(),
            certs,
        }
    }

    /// The health cell, with what the last probe said as its tooltip.
    fn health_pill(&self) -> Pill {
        let pill = Pill::from(self.health);
        match &self.health_detail {
            Some(detail) => pill.title(detail),
            None => pill,
        }
    }
}

/// The certificate of one routed host.
#[derive(Debug)]
struct CertCell {
    host: String,
    /// How long it is still valid (`12 d`, `expired`); `None` while none is served.
    left: Option<String>,
    /// The end of validity, RFC 3339.
    not_after: String,
    /// `Valid`, `Expiring` or `Expired`; `Pending` while none is served.
    state: CertState,
}

impl CertCell {
    fn new(host: &str, cert: Option<&CertExpiry>, now: SystemTime) -> Self {
        let left = cert.map(|cert| cert.remaining(now));
        CertCell {
            host: host.to_owned(),
            left: left.map(Self::left),
            not_after: cert
                .map(|cert| humantime::format_rfc3339_seconds(cert.not_after).to_string())
                .unwrap_or_default(),
            state: left.map_or(CertState::Pending, CertState::from),
        }
    }

    /// The served certificate's time left as a pill with its end of validity as the tooltip;
    /// `None` when it is still valid for long (plain text) or none is served.
    fn pill(&self) -> Option<Pill> {
        let left = self.left.as_ref()?;
        Some(self.state.pill(left)?.title(&self.not_after))
    }

    /// `left` in words: `expired`, or how long.
    fn left(left: Duration) -> String {
        if left.is_zero() {
            "expired".to_owned()
        } else {
            format!("{} left", Elapsed(left))
        }
    }
}

/// The statistics as the dashboard shows them.
#[derive(Debug, Default)]
pub struct StatsView {
    pub routes: Vec<TrafficRow>,
    /// Requests that matched no route.
    pub unrouted: TrafficRow,
    pub deployments: Vec<DeployRow>,
    pub services: Vec<ServiceRestarts>,
    pub certificates: Vec<CertRow>,
    pub problems: Vec<String>,
}

/// One route's traffic.
#[derive(Debug, Default)]
pub struct TrafficRow {
    /// `host/prefix`.
    pub route: String,
    /// Who serves it: "web (blog)", "hoster (admin UI)".
    pub owner: String,
    pub statuses: Vec<(StatusClass, u64)>,
    /// Every finished request.
    pub requests: u64,
    pub upstream_errors: u64,
    pub inflight: u64,
    pub p50: String,
    pub p95: String,
}

/// One stack's deployments.
#[derive(Debug)]
pub struct DeployRow {
    pub stack: String,
    pub succeeded: u32,
    pub failed: u32,
    pub skipped: u32,
    pub mean: String,
    pub max: String,
}

/// One served certificate.
#[derive(Debug)]
pub struct CertRow {
    pub host: String,
    pub not_after: String,
    pub left: String,
    pub state: CertState,
}

impl StatsView {
    /// The status classes, in the order of [`TrafficRow::statuses`].
    pub fn classes(&self) -> [StatusClass; 6] {
        StatusClass::ALL
    }

    /// How far back the latency percentiles look, e.g. `5 min`.
    pub fn latency_window(&self) -> String {
        Elapsed(Stats::LATENCY_WINDOW).to_string()
    }

    /// How far back the deployment statistics look, e.g. `30 d`.
    pub fn deploy_history(&self) -> String {
        Elapsed(Stats::DEPLOY_HISTORY).to_string()
    }

    /// The rows of the traffic table: every route, then the unrouted requests.
    pub fn traffic(&self) -> impl Iterator<Item = &TrafficRow> {
        self.routes.iter().chain([&self.unrouted])
    }
}

impl TrafficRow {
    fn new(route: String, owner: String, counts: &Counts) -> Self {
        TrafficRow {
            route,
            owner,
            statuses: counts.statuses.iter().collect(),
            requests: counts.statuses.total(),
            upstream_errors: counts.upstream_errors,
            inflight: counts.inflight,
            p50: Elapsed::cell(counts.latency.p50),
            p95: Elapsed::cell(counts.latency.p95),
        }
    }
}

impl From<Snapshot> for StatsView {
    fn from(snapshot: Snapshot) -> Self {
        let routes = snapshot
            .traffic
            .routes
            .iter()
            .map(|route| {
                let owner = match &route.owner {
                    Some(Owner::Route(owner)) => format!("{} ({})", owner.service, owner.stack),
                    Some(Owner::Reserved(purpose)) => format!("hoster ({purpose})"),
                    None => String::new(),
                };
                let name = format!("{}{}", route.host, route.prefix);
                TrafficRow::new(name, owner, &route.counts)
            })
            .collect();
        let unrouted = TrafficRow::new(
            "unrouted".to_owned(),
            String::new(),
            &snapshot.traffic.unrouted,
        );
        let deployments = snapshot
            .deployments
            .iter()
            .map(|stack| DeployRow {
                stack: stack.stack.clone(),
                succeeded: stack.succeeded(),
                failed: stack.failed(),
                skipped: stack.skipped(),
                mean: Elapsed::cell(stack.mean_live_duration()),
                max: Elapsed::cell(stack.max_live_duration()),
            })
            .collect();
        let certificates = snapshot
            .certificates
            .iter()
            .map(|cert| {
                let left = cert.remaining(snapshot.taken_at);
                CertRow {
                    host: cert.host.clone(),
                    not_after: humantime::format_rfc3339_seconds(cert.not_after).to_string(),
                    left: if left.is_zero() {
                        "expired".to_owned()
                    } else {
                        Elapsed(left).to_string()
                    },
                    state: CertState::from(left),
                }
            })
            .collect();
        StatsView {
            routes,
            unrouted,
            deployments,
            services: snapshot.services,
            certificates,
            problems: snapshot.problems,
        }
    }
}

#[cfg(test)]
mod tests;
