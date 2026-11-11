//! `GET /stacks`: every stack as a card, under a status filter (ADR-0054, epic 8bf).
//!
//! Each stack gets one [`Condition`] (deploying, failed, degraded, running, or not deployed yet),
//! shown as its pill and counted in the filter above the cards. The filter is a row of links
//! that set `?status=`; the page is rendered on the server for the chosen condition, so it needs
//! no JavaScript. An unknown `status` shows every stack.
//!
//! A card shows the stack's revision and content hash, its running or latest deployment (with
//! the commit and repository of a webhook deploy's OIDC provenance, ADR-0047), its services as
//! chips with a health dot, the hosts its routes serve, and a small row of request statistics
//! from the traffic history over the [`TrafficWindow`] (the last hour, ADR-0062 §8), all the
//! stack's routes together: requests per minute over the last
//! [`ScopeHistory::RATE_WINDOW`], the p95 and the share of 5xx responses, and a sparkline of
//! its requests per minute in the tone of its condition. The history of every stack is read
//! once per render; it lags by up to a minute, as it holds only flushed minutes.
//!
//! Under the cards, the managed containers whose stack label names no stack hoster has (left
//! behind by a removal that did not finish, or made by hand with hoster's labels) are listed
//! with a link to their log, since no stack page shows them.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use askama::Template;
use axum::Extension;
use axum::extract::Query;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{DeploymentView, Outcome, ShortDigest, StackView, Stacks, When};
use crate::health::HealthStatus;
use crate::orchestrator::{ServiceState, ServiceStatus, StackStatus};
use crate::runtime::{ContainerInfo, label};
use crate::stack::Stack;
use crate::stats::{Elapsed, Range, ScopeHistory, Stats, TrafficHistory, TrafficTotals};
use crate::store::{Deployment, DeploymentSource, DeploymentState, Timestamp};
use crate::web::charts::Sparkline;
use crate::web::containers::Containers;
use crate::web::pages::{Layout, Page, Section, Shell};
use crate::web::ui::{Count, PerMinute, Pill, Tone};

impl Stacks {
    /// `GET /stacks[?status=…]`: every stack, or those in one [`Condition`].
    pub async fn list(
        Extension(stacks): Extension<Stacks>,
        Extension(stats): Extension<Stats>,
        Extension(containers): Extension<Containers>,
        shell: Shell,
        Query(query): Query<ListQuery>,
    ) -> Response {
        let layout = shell.layout(Section::Stacks);
        let filter = query.status.as_deref().and_then(|s| s.parse().ok());
        let (list, history) = tokio::join!(stacks.0.status(), stats.history(TrafficWindow::RANGE));
        // The cards say so when the history cannot be read; the page still serves.
        let history = history
            .inspect_err(|err| {
                tracing::warn!(
                    error = format!("{err:#}"),
                    "cannot read the traffic history"
                );
            })
            .ok();
        match list {
            Ok(list) => {
                let managed = containers.managed().await.unwrap_or_else(|err| {
                    tracing::warn!(error = %err, "listing the containers failed");
                    Vec::new()
                });
                let updates = stacks.update_counts().await;
                Page(
                    StackList::new(layout, &list, history.as_ref(), filter, SystemTime::now())
                        .with_leftovers(&list, &managed)
                        .with_updates(&updates),
                )
                .into_response()
            }
            Err(err) => {
                let outcome = Outcome::from(err);
                let status = outcome.status;
                let page = StackList::new(
                    layout.with_flash(outcome.into()),
                    &[],
                    history.as_ref(),
                    filter,
                    SystemTime::now(),
                );
                (status, Page(page)).into_response()
            }
        }
    }
}

/// The query of `GET /stacks`.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    status: Option<String>,
}

/// `GET /stacks`.
#[derive(Template)]
#[template(path = "stacks.html")]
pub struct StackList {
    layout: Layout,
    /// The filter links, `all` first; empty when there are no stacks at all.
    filters: Vec<FilterLink>,
    /// The stacks the filter lets through.
    cards: Vec<StackCard>,
    /// The condition filtered on, if one is.
    filter: Option<Condition>,
    /// Managed containers of stacks that are gone.
    leftovers: Vec<Leftover>,
}

impl StackList {
    /// Marks each card with the number of updates its stack is offered, majors aside
    /// (ADR-0061 §9).
    fn with_updates(mut self, updates: &BTreeMap<String, usize>) -> Self {
        for card in &mut self.cards {
            card.updates = updates.get(&card.name).copied().unwrap_or_default();
        }
        self
    }

    /// The list of `stacks` under `filter`, with their traffic from `history` (over
    /// [`TrafficWindow::RANGE`]; `None` when it cannot be read).
    fn new(
        layout: Layout,
        stacks: &[StackStatus],
        history: Option<&TrafficHistory>,
        filter: Option<Condition>,
        now: SystemTime,
    ) -> Self {
        let conditions: Vec<Condition> = stacks.iter().map(Condition::from).collect();
        let filters = if stacks.is_empty() {
            Vec::new()
        } else {
            std::iter::once(None)
                .chain(Condition::ALL.into_iter().map(Some))
                .filter_map(|condition| {
                    let count = conditions
                        .iter()
                        .filter(|c| condition.is_none_or(|wanted| **c == wanted))
                        .count();
                    // "not deployed" is rare; its link only shows while a stack is in it, or
                    // while it is the filter chosen, so the chosen filter is always marked.
                    (condition != Some(Condition::NotDeployed) || count > 0 || condition == filter)
                        .then(|| FilterLink {
                            label: condition.map_or_else(|| "all".to_owned(), |c| c.to_string()),
                            href: condition.map_or_else(
                                || "/stacks".to_owned(),
                                |c| format!("/stacks?status={}", c.slug()),
                            ),
                            count,
                            current: condition == filter,
                        })
                })
                .collect()
        };
        let cards = stacks
            .iter()
            .zip(conditions)
            .filter(|(_, condition)| filter.is_none_or(|wanted| *condition == wanted))
            .map(|(status, condition)| StackCard::new(status, condition, history, now))
            .collect();
        StackList {
            layout,
            filters,
            cards,
            filter,
            leftovers: Vec::new(),
        }
    }

    /// The list with the `managed` containers whose stack is none of `stacks`.
    fn with_leftovers(mut self, stacks: &[StackStatus], managed: &[ContainerInfo]) -> Self {
        self.leftovers = managed
            .iter()
            .filter(|c| {
                c.labels
                    .get(label::STACK)
                    .is_none_or(|stack| !stacks.iter().any(|s| &s.name == stack))
            })
            .map(Leftover::from)
            .collect();
        self.leftovers.sort_by(|a, b| a.name.cmp(&b.name));
        self
    }
}

/// A managed container whose stack hoster no longer has.
#[derive(Debug)]
struct Leftover {
    name: String,
    /// The stack and service its labels name.
    stack: String,
    service: String,
    /// Its state, as podman names it.
    state: String,
    logs: String,
}

impl From<&ContainerInfo> for Leftover {
    fn from(container: &ContainerInfo) -> Self {
        let label = |key| container.labels.get(key).cloned().unwrap_or_default();
        Self {
            name: container.name.clone(),
            stack: label(label::STACK),
            service: label(label::SERVICE),
            state: container.state.to_string(),
            logs: format!("/containers/{}/logs", container.name),
        }
    }
}

/// One link of the status filter.
#[derive(Debug)]
struct FilterLink {
    label: String,
    href: String,
    count: usize,
    current: bool,
}

/// Where a stack stands, as its pill and the status filter name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// Every service runs, none fails its health check.
    Running,
    /// Something runs but not all is well: a service failing or crash looping, a service not
    /// running beside others that do, or the last deployment went live with warnings.
    Degraded,
    /// A deployment is under way.
    Deploying,
    /// The latest deployment failed or was rejected.
    Failed,
    /// Nothing runs yet, and nothing failed: a stack waiting for its first digest.
    NotDeployed,
}

impl Condition {
    /// Every condition, in the filter's order.
    pub const ALL: [Condition; 5] = [
        Condition::Running,
        Condition::Degraded,
        Condition::Deploying,
        Condition::Failed,
        Condition::NotDeployed,
    ];

    /// The `?status=` value.
    fn slug(self) -> &'static str {
        match self {
            Condition::Running => "running",
            Condition::Degraded => "degraded",
            Condition::Deploying => "deploying",
            Condition::Failed => "failed",
            Condition::NotDeployed => "not-deployed",
        }
    }

    /// The stack's pill.
    pub(in crate::web) fn pill(self) -> Pill {
        Tone::from(self).pill(self)
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Condition::NotDeployed => "not deployed",
            other => other.slug(),
        })
    }
}

impl FromStr for Condition {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Condition::ALL
            .into_iter()
            .find(|c| c.slug() == s)
            .ok_or_else(|| format!("no stack status called {s:?}"))
    }
}

impl From<Condition> for Tone {
    fn from(condition: Condition) -> Self {
        match condition {
            Condition::Running => Tone::Ok,
            Condition::Degraded => Tone::Warn,
            Condition::Deploying => Tone::Progress,
            Condition::Failed => Tone::Fail,
            Condition::NotDeployed => Tone::Neutral,
        }
    }
}

impl From<&StackStatus> for Condition {
    /// A running deployment comes first, then a failed one, then the services.
    fn from(status: &StackStatus) -> Self {
        let last = status.last.as_ref().map(|last| last.state);
        if status.active.is_some() {
            return Condition::Deploying;
        }
        if status.flag.is_some() || last.is_some_and(|state| Tone::from(state) == Tone::Fail) {
            return Condition::Failed;
        }
        let running = status
            .services
            .iter()
            .filter(|s| matches!(s.state, ServiceState::Running { .. }))
            .count();
        let failing = status.services.iter().any(ServiceChip::failing);
        if running == 0 && !failing {
            Condition::NotDeployed
        } else if failing
            || running < status.services.len()
            || last == Some(DeploymentState::LiveWithWarnings)
        {
            Condition::Degraded
        } else {
            Condition::Running
        }
    }
}

/// One stack's card.
#[derive(Debug)]
pub(super) struct StackCard {
    name: String,
    url: String,
    condition: Condition,
    revision: u32,
    /// The active revision's content hash, shortened, and in full.
    hash: String,
    full_hash: String,
    /// "revision N: deploy failed: …" (ADR-0049).
    flag: Option<String>,
    /// The deployment running now, as (id, state, url).
    active: Option<(String, String, String)>,
    /// The latest deployment, when none runs.
    last: Option<LastDeploy>,
    queued: Option<String>,
    services: Vec<ServiceChip>,
    /// `host` or `host/prefix` of every route, sorted.
    routes: Vec<String>,
    /// Its traffic over the [`TrafficWindow`].
    traffic: CardTraffic,
    /// Updates on offer, majors aside (ADR-0061 §9).
    updates: usize,
}

impl StackCard {
    /// The card of `status`, with its traffic from `history` (`None` when it cannot be read).
    fn new(
        status: &StackStatus,
        condition: Condition,
        history: Option<&TrafficHistory>,
        now: SystemTime,
    ) -> Self {
        let hash = status.revision.hash.to_string();
        let deploying = Self::deploying(status);
        let routes = Self::routes(status);
        StackCard {
            url: StackView::url(&status.name),
            name: status.name.clone(),
            condition,
            revision: status.revision.revision,
            hash: hash.get(..12).unwrap_or(&hash).to_owned(),
            full_hash: hash.clone(),
            flag: status.flag.clone(),
            active: status.active.map(|(id, state)| {
                let state = crate::web::ui::StateLabel(state);
                (
                    id.to_string(),
                    state.to_string(),
                    DeploymentView::url(&status.name, id),
                )
            }),
            last: status
                .last
                .as_ref()
                .filter(|_| status.active.is_none())
                .map(|last| LastDeploy::new(last, now)),
            queued: status.queued.map(|id| id.to_string()),
            services: status
                .services
                .iter()
                .map(|service| {
                    let moving = deploying
                        .as_ref()
                        .is_some_and(|names| names.contains(service.name.as_str()));
                    ServiceChip::new(service, moving)
                })
                .collect(),
            traffic: match history {
                _ if routes.is_empty() => CardTraffic::NoRoutes,
                None => CardTraffic::Unreadable,
                Some(history) => {
                    CardTraffic::Shown(RowTraffic::new(history.stack(&status.name), condition))
                }
            },
            routes,
            updates: 0,
        }
    }

    /// The services the running deployment moves, if one runs. A deploy names the services it
    /// moves; one whose target is not known moves them all.
    pub(super) fn deploying(status: &StackStatus) -> Option<BTreeSet<&str>> {
        status.active.map(
            |(id, _)| match status.last.as_ref().filter(|last| last.id == id) {
                Some(last) => last.target.keys().map(String::as_str).collect(),
                None => status.services.iter().map(|s| s.name.as_str()).collect(),
            },
        )
    }

    /// The routes of the active revision. Empty when the revision does not parse (it did when
    /// it was applied, so only a newer hoster could refuse it).
    pub(super) fn routes(status: &StackStatus) -> Vec<String> {
        let Ok(stack) = status.revision.toml.parse::<Stack>() else {
            return Vec::new();
        };
        let routes: BTreeSet<String> = stack
            .services
            .values()
            .flat_map(|service| &service.routes)
            .map(|route| {
                if route.prefix.is_root() {
                    route.host.to_string()
                } else {
                    format!("{}{}", route.host, route.prefix)
                }
            })
            .collect();
        routes.into_iter().collect()
    }
}

/// What a card shows of its stack's traffic.
#[derive(Debug)]
enum CardTraffic {
    /// The stack routes nothing, so there is no traffic to count.
    NoRoutes,
    /// The traffic history cannot be read.
    Unreadable,
    Shown(RowTraffic),
}

/// The traffic the stack pages show, which have no range picker (ADR-0062 §8): the cards on
/// `/stacks` and the stack overview. Its `Display` is how their labels name it (`last hour`),
/// so changing [`Self::RANGE`] changes both pages and their labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TrafficWindow;

impl TrafficWindow {
    /// The range read for the stack pages.
    pub(super) const RANGE: Range = Range::Hour;
}

impl fmt::Display for TrafficWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match Self::RANGE {
            Range::Hour => f.write_str("last hour"),
            range => write!(f, "last {range}"),
        }
    }
}

/// A stack's traffic over a range of the history, in its row of the dashboard's stacks table
/// or on its card: the current rate, the figures over the range and a sparkline.
#[derive(Debug)]
pub(in crate::web) struct RowTraffic {
    /// Requests per minute over the last [`ScopeHistory::RATE_WINDOW`].
    pub(in crate::web) per_minute: PerMinute,
    /// The figures over the range.
    pub(in crate::web) line: TrafficLine,
    /// Its requests over the range, in the tone of the stack's condition.
    pub(in crate::web) spark: Sparkline,
}

impl RowTraffic {
    /// The traffic of `scope`, a stack in `condition`.
    pub(in crate::web) fn new(scope: &ScopeHistory, condition: Condition) -> Self {
        RowTraffic {
            per_minute: PerMinute(scope.per_minute()),
            line: TrafficLine::from(&scope.totals),
            spark: Sparkline::new(
                scope.bars.iter().map(|b| b.traffic.requests()),
                Tone::from(condition),
            ),
        }
    }

    /// The requests over the range, for the sparkline in words.
    pub(in crate::web) fn requests(&self) -> Count {
        Count(self.line.requests)
    }
}

/// A stack's latest deployment, when none runs.
#[derive(Debug)]
pub(in crate::web) struct LastDeploy {
    pub(in crate::web) id: String,
    pub(in crate::web) url: String,
    pub(in crate::web) state: Pill,
    /// `manual`, `webhook`, `poller`, `apply`, `rollback`.
    pub(in crate::web) source: String,
    /// How long ago it last changed, and the time itself.
    pub(in crate::web) ago: String,
    pub(in crate::web) at: String,
    /// A webhook deploy's commit.
    pub(in crate::web) commit: Option<Commit>,
}

impl LastDeploy {
    pub(in crate::web) fn new(deployment: &Deployment, now: SystemTime) -> Self {
        LastDeploy {
            id: deployment.id.to_string(),
            url: DeploymentView::url(&deployment.stack, deployment.id),
            state: Pill::from(deployment.state),
            source: Source(deployment.source).to_string(),
            ago: Ago::since(deployment.updated_at, now).to_string(),
            at: When(deployment.updated_at).to_string(),
            commit: Commit::of(deployment),
        }
    }
}

/// The commit a webhook deploy was built from, from its provenance (ADR-0047).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::web) struct Commit {
    /// The sha's first seven characters.
    pub(in crate::web) short: String,
    pub(in crate::web) sha: String,
    pub(in crate::web) repository: String,
}

impl Commit {
    /// The commit of `deployment`, if a webhook started it.
    pub(in crate::web) fn of(deployment: &Deployment) -> Option<Self> {
        deployment
            .provenance
            .as_ref()
            .filter(|_| deployment.source == DeploymentSource::Webhook)
            .map(|p| Commit {
                short: p.sha.get(..7).unwrap_or(&p.sha).to_owned(),
                sha: p.sha.clone(),
                repository: p.repository.clone(),
            })
    }
}

/// A deployment's source in lower case, as the cards write it.
pub(super) struct Source(pub(super) DeploymentSource);

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            DeploymentSource::Webhook => "webhook",
            DeploymentSource::Poller => "poller",
            DeploymentSource::Apply => "apply",
            DeploymentSource::Manual => "manual",
            DeploymentSource::Rollback => "rollback",
            DeploymentSource::AutoUpdate => "auto-update",
        })
    }
}

/// How long ago something happened, in its largest unit: `just now`, `5 min ago`, `3 h ago`,
/// `12 d ago`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ago(pub Duration);

impl Ago {
    /// The time from `then` to `now`; zero when the clock went backwards.
    pub(in crate::web) fn since(then: Timestamp, now: SystemTime) -> Self {
        Ago(now
            .duration_since(SystemTime::from(then))
            .unwrap_or_default())
    }
}

impl fmt::Display for Ago {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        match secs {
            0..60 => f.write_str("just now"),
            60..3_600 => write!(f, "{} min ago", secs / 60),
            3_600..86_400 => write!(f, "{} h ago", secs / 3_600),
            _ => write!(f, "{} d ago", secs / 86_400),
        }
    }
}

/// A service on a card: its name with a health dot.
#[derive(Debug)]
pub(super) struct ServiceChip {
    name: String,
    /// The dot's colour: ok, fail (failing), progress (deploying), neutral (stopped).
    tone: Tone,
    /// The dot in words.
    word: &'static str,
    /// The tooltip: what runs, its digest and health.
    detail: String,
}

impl ServiceChip {
    fn new(service: &ServiceStatus, deploying: bool) -> Self {
        let (tone, word) = if Self::failing(service) {
            (Tone::Fail, "failing")
        } else if deploying {
            (Tone::Progress, "deploying")
        } else if matches!(service.state, ServiceState::Running { .. }) {
            (Tone::Ok, "ok")
        } else if service.state == ServiceState::AwaitingFirstDigest {
            (Tone::Neutral, "waiting")
        } else {
            (Tone::Neutral, "stopped")
        };
        let detail = match &service.state {
            ServiceState::Running { generation, digest } => {
                let health = match service.health.status {
                    _ if service.restarts.crash_looping => "crash looping".to_owned(),
                    HealthStatus::Unknown => "health unknown".to_owned(),
                    status => status.to_string(),
                };
                format!("generation {generation}, {}, {health}", ShortDigest(digest))
            }
            ServiceState::AwaitingFirstDigest => "awaiting its first digest".to_owned(),
            ServiceState::NotDeployed => "not deployed".to_owned(),
        };
        ServiceChip {
            name: service.name.clone(),
            tone,
            word,
            detail,
        }
    }

    /// The dot in words, shown next to the name unless the service is fine, so a state never
    /// rests on colour alone (WCAG 1.4.1).
    fn shown_word(&self) -> Option<&'static str> {
        (self.tone != Tone::Ok).then_some(self.word)
    }

    /// The chip for screen readers, which cannot reach the tooltip: the dot in words unless it
    /// is already shown, then the detail.
    fn spoken(&self) -> String {
        match self.shown_word() {
            Some(_) => self.detail.clone(),
            None => format!("{}, {}", self.word, self.detail),
        }
    }

    /// A running service whose probe fails, or that keeps crashing (ADR-0028, ADR-0038).
    pub(super) fn failing(service: &ServiceStatus) -> bool {
        service.is_crash_looping()
            || (matches!(service.state, ServiceState::Running { .. })
                && service.health.status == HealthStatus::Unhealthy)
    }
}

/// A stack's request statistics over a span of the traffic history (ADR-0062), all its routes
/// together: the stack cards, the stack overview's KPI tiles and the dashboard.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::web) struct TrafficLine {
    /// Finished requests.
    pub(in crate::web) requests: u64,
    /// Of those, the 5xx responses.
    pub(in crate::web) server_errors: u64,
    /// The p95 of the same requests, `-` without any.
    pub(in crate::web) p95: String,
    pub(in crate::web) p95_tone: Tone,
    /// The p50 of the same requests, `-` without any.
    pub(in crate::web) p50: String,
    /// The share of 4xx responses, `-` without requests.
    pub(in crate::web) client_errors: String,
    /// The share of 5xx responses, `-` without requests.
    pub(in crate::web) errors: String,
    pub(in crate::web) errors_tone: Tone,
}

impl TrafficLine {
    /// A p95 this slow is flagged.
    const SLOW: Duration = Duration::from_secs(1);
    /// A 5xx share of this many per mille or more is flagged.
    const FAILING_PER_MILLE: u64 = 10;

    /// The figures of `requests` with these 4xx and 5xx counts and percentiles.
    fn new(
        requests: u64,
        client_errors: u64,
        server_errors: u64,
        p50: Option<Duration>,
        p95: Option<Duration>,
    ) -> Self {
        TrafficLine {
            requests,
            server_errors,
            p50: Elapsed::cell(p50),
            p95: Elapsed::cell(p95),
            p95_tone: if p95.is_some_and(|p| p >= Self::SLOW) {
                Tone::Warn
            } else {
                Tone::Neutral
            },
            client_errors: Share(client_errors, requests).to_string(),
            errors: Share(server_errors, requests).to_string(),
            errors_tone: if requests > 0
                && server_errors * 1000 >= requests * Self::FAILING_PER_MILLE
            {
                Tone::Fail
            } else {
                Tone::Neutral
            },
        }
    }
}

/// The figures of a span of the traffic history (ADR-0062).
impl From<&TrafficTotals> for TrafficLine {
    fn from(totals: &TrafficTotals) -> Self {
        Self::new(
            totals.requests(),
            totals.client_errors(),
            totals.server_errors(),
            totals.p50(),
            totals.p95(),
        )
    }
}

/// A part of a whole as a percentage for a dashboard: `2.0 %`, `0 %`, `<0.1 %` for a share
/// that rounds to nothing but is not none, and `-` of nothing.
struct Share(u64, u64);

impl fmt::Display for Share {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Share(part, whole) = *self;
        if whole == 0 {
            f.write_str("-")
        } else if part == 0 {
            f.write_str("0 %")
        } else {
            // The cast loses nothing below 2^53 requests.
            match format!("{:.1}", part as f64 * 100.0 / whole as f64) {
                zero if zero == "0.0" => f.write_str("<0.1 %"),
                share => write!(f, "{share} %"),
            }
        }
    }
}

#[cfg(test)]
mod tests;
