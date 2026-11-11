//! The dashboard at `/` (spec §5.4, ADR-0021, ADR-0045, ADR-0054), from data hoster has.
//!
//! Its traffic figures come from the persisted traffic history (ADR-0062) over the [`Range`]
//! the header's picker sets (`?range=1h` … `?range=30d`, 24h by default and for an unknown
//! value). They cover every stack's routes and the unrouted requests, without hoster's own
//! routes (the admin UI, whose live pages would otherwise weigh on a quiet host's figures), and
//! end with the newest flushed minute, so they lag by up to a minute:
//!
//! - **KPI tiles**: the requests over the range (with the requests in flight now, the one live
//!   figure), their 5xx rate (`n of m`), and their p50 and p95 latency. The requests, p50 and
//!   p95 tiles say how they changed against the period of equal length just before the range,
//!   when that period has data; the change keeps a neutral tone. Then stacks running out of all
//!   (with how many are degraded or deploying) and certificates valid out of all (with how many
//!   are failing or expiring). The 5xx, p95, stacks and certificates tiles take the tone of the
//!   worst state they count.
//! - **Warnings**, v1's only alerting surface, as an alert list shown only while there are any.
//! - **Charts**: responses by status as stacked bars and the p50/p95 response time as lines,
//!   one bar or point per bucket of the range's bar plan ([`crate::web::charts`]).
//! - **Stacks**: one row per stack with its [`Condition`] pill, healthy services out of all, its
//!   requests per minute over the last [`ScopeHistory::RATE_WINDOW`], p50, p95 and its 4xx and
//!   5xx shares over the range, a sparkline of its requests over the range in the tone of its
//!   condition, the restarts of its running services (ADR-0028), its deployments of the last
//!   [`Stats::DEPLOY_HISTORY`] and its running or latest deployment. Service detail (digests,
//!   per-host certificates) is on the stack page. Under the table, a note counts the requests
//!   over the range no stack row shows (unrouted ones, hoster's own routes and the routes of
//!   removed stacks, with their upstream errors) and the deployments of removed stacks.
//! - **Certificates**: the hosts soonest to expire with a days-left meter (out of 90) and their
//!   status, every host needing attention among them, and how many more are valid.
//! - **Recent events**: the newest events of the event log (ADR-0055), as [`RecentEvents`].
//!
//! The page renders server-side and works without JavaScript. Everything under the header is
//! one partial, [`Overview`]: the page holds it in a live region that follows `GET /events` over
//! SSE, with the range in the stream's URL so the partials it sends stay on it. The stream sends
//! the partial again whenever the orchestrator reports a change on `ui_events` (a health
//! change, a certificate issued or failed, a deployment moving) or the receiver lagged, and at
//! least every [`OverviewEvents::REFRESH`] for what no event reports (each minute's traffic
//! flush, restart counts, a removed stack, certificates nearing expiry with time). A burst of
//! nothing but newly recorded events ([`UiEvent::Event`]) sends only the Recent events card, with
//! them added to the one the stream sent last, as an `<hx-partial>` that replaces just that card
//! ([`RecentEvents::partial`]); the overview's other parts do not change with it. It ends
//! with an `end` event when the session ends (after replacing the overview with a notice, so a
//! page signed out elsewhere does not keep showing a stale status as if it were live) or the
//! orchestrator is gone, which closes the connection.
//!
//! A warning is raised for a live service whose health probe fails, a service in a crash loop
//! (by the health monitor or the reconciler, ADR-0028), a served certificate that expires within
//! [`CertState::WARN_WITHIN`], and a stack whose latest deployment failed or was rejected.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use askama::Template;
use axum::Extension;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast;

use super::auth::{Auth, Session};
use super::certificates::{CertGlance, Certificates};
use super::charts::{LineChart, Percentiles, StackedBars, Timeline};
use super::events::RecentEvents;
use super::icons::Icon;
use super::pages::{Layout, Page, Section, Shell};
use super::stacks::{
    Condition, DeploymentView, LastDeploy, RowTraffic, StackView, Stacks, TrafficLine,
};
use super::ui::{CertState, Count, Kpi, Pill, Tone};
use crate::health::HealthStatus;
use crate::orchestrator::{ServiceState, ServiceStatus, StackStatus, UiEvent};
use crate::stack::Stack;
use crate::stats::{
    CertExpiry, DeployStats, Elapsed, Range, ScopeHistory, Stats, StatusClass, TrafficHistory,
    TrafficSnapshot, TrafficTotals,
};
use crate::store::{Event as LoggedEvent, EventKind, Timestamp};

/// The dashboard: the live overview.
#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct Dashboard {
    layout: Layout,
    /// The range the traffic figures cover.
    range: Range,
    overview: Overview,
}

impl Dashboard {
    /// The URL of the overview's live stream.
    pub const EVENTS: &str = "/events";

    /// The `GET /[?range=…]` handler.
    pub async fn show(
        shell: Shell,
        Query(query): Query<RangeQuery>,
        Extension(stats): Extension<Stats>,
        Extension(stacks): Extension<Stacks>,
        Extension(certificates): Extension<Certificates>,
    ) -> Page<Dashboard> {
        let range = Range::from(query);
        let readings = Readings::gather(&stacks, &stats, &certificates, range).await;
        Page(Dashboard {
            layout: shell.layout(Section::Dashboard),
            range,
            overview: Overview::new(readings, SystemTime::now()),
        })
    }

    /// `GET /events[?range=…]`: the overview partial over the range as `text/event-stream`,
    /// sent at once and again on every change the orchestrator reports, until the session
    /// ends.
    pub async fn events(
        State(auth): State<Arc<Auth>>,
        Extension(session): Extension<Session>,
        Query(query): Query<RangeQuery>,
        Extension(stats): Extension<Stats>,
        Extension(stacks): Extension<Stacks>,
        Extension(certificates): Extension<Certificates>,
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
        let live = OverviewEvents {
            stacks,
            stats,
            certificates,
            range: Range::from(query),
        };
        Sse::new(live.stream(events, signed_out))
            .keep_alive(KeepAlive::new().interval(OverviewEvents::KEEP_ALIVE))
            .into_response()
    }

    /// The stream's URL, on the page's range.
    pub fn events_url(&self) -> String {
        format!("{}?range={}", Self::EVENTS, self.range)
    }

    /// The range picker: a link per range, shortest first, the page's own marked.
    fn ranges(&self) -> impl Iterator<Item = (Range, String, bool)> + '_ {
        Range::ALL
            .into_iter()
            .map(|range| (range, format!("/?range={range}"), range == self.range))
    }

    /// The page with no stacks and no statistics, for the layout tests.
    #[cfg(test)]
    pub(super) fn empty(layout: Layout) -> Self {
        Dashboard {
            layout,
            range: Range::default(),
            overview: Overview::new(Readings::default(), SystemTime::now()),
        }
    }
}

/// The query of `GET /` and `GET /events`: the traffic range.
#[derive(Debug, Default, Deserialize)]
pub struct RangeQuery {
    range: Option<String>,
}

/// The range asked for; [`Range::default`] without one or for one that is none of
/// [`Range::ALL`].
impl From<RangeQuery> for Range {
    fn from(query: RangeQuery) -> Self {
        query
            .range
            .and_then(|range| range.parse().ok())
            .unwrap_or_default()
    }
}

/// The overview as SSE events.
struct OverviewEvents {
    stacks: Stacks,
    stats: Stats,
    certificates: Certificates,
    /// The range of the page that opened the stream.
    range: Range,
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
    /// [`Self::SIGNED_OUT`] first); then `end`. A burst of only recorded events sends the
    /// Recent events card alone, from the one sent last.
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
        // The state holds the card last sent, `None` before the first update.
        let updates = stream::unfold(Some((self, events, None)), |state| async move {
            let (this, mut events, recent): (Self, _, Option<RecentEvents>) = state?;
            let (event, recent) = match recent {
                None => this.render().await,
                Some(mut recent) => match Self::changed(&mut events).await {
                    Change::Closed => return None,
                    Change::Events(new) => {
                        if recent.add(new) {
                            let card = recent.partial(SystemTime::now());
                            (Event::default().data(card), recent)
                        } else {
                            // The card could not read the events before, so it has none to
                            // add to.
                            this.render().await
                        }
                    }
                    Change::All => this.render().await,
                },
            };
            Some((event, Some((this, events, Some(recent)))))
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
    /// probes) renders once. A lagged receiver may have missed events, so it re-reads all.
    async fn changed(events: &mut broadcast::Receiver<UiEvent>) -> Change {
        let mut recorded = Vec::new();
        let mut all = false;
        match tokio::time::timeout(Self::REFRESH, events.recv()).await {
            // Nothing reported a change, but time passed (certificate expiry, restarts).
            Err(_) => return Change::All,
            Ok(Ok(UiEvent::Event(event))) => {
                all |= Self::changes_overview(event.kind);
                recorded.push(event);
            }
            Ok(Ok(_) | Err(broadcast::error::RecvError::Lagged(_))) => all = true,
            Ok(Err(broadcast::error::RecvError::Closed)) => return Change::Closed,
        }
        loop {
            match events.try_recv() {
                Ok(UiEvent::Event(event)) => {
                    all |= Self::changes_overview(event.kind);
                    recorded.push(event);
                }
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => all = true,
                Err(broadcast::error::TryRecvError::Empty) if all => return Change::All,
                Err(broadcast::error::TryRecvError::Empty) => return Change::Events(recorded),
                Err(broadcast::error::TryRecvError::Closed) => return Change::Closed,
            }
        }
    }

    /// Whether an event of `kind` is the only report of a change elsewhere on the overview
    /// (the restart counts, a stack's condition, a removed stack's row), so it needs a fresh
    /// read rather than the card alone.
    fn changes_overview(kind: EventKind) -> bool {
        matches!(
            kind,
            EventKind::ContainerRestarted
                | EventKind::CrashLoopEntered
                | EventKind::CrashLoopLeft
                | EventKind::StackRemoved
        )
    }

    /// The whole overview, read now, and its Recent events card for the next events.
    async fn render(&self) -> (Event, RecentEvents) {
        let readings =
            Readings::gather(&self.stacks, &self.stats, &self.certificates, self.range).await;
        let overview = Overview::new(readings, SystemTime::now());
        let html = overview.render().unwrap_or_else(|err| {
            tracing::error!(error = %err, "rendering the dashboard overview failed");
            String::new()
        });
        (Event::default().data(html), overview.recent)
    }
}

/// What a burst of [`UiEvent`]s changed on the overview.
#[derive(Debug)]
enum Change {
    /// Something only a fresh read shows: a state change, time passing, a lagged receiver.
    All,
    /// Only these events were recorded, oldest first.
    Events(Vec<LoggedEvent>),
    /// The orchestrator is gone: nothing changes any more.
    Closed,
}

/// What the overview is made from, read at one moment.
#[derive(Debug)]
struct Readings {
    /// Every stack, or why they cannot be read.
    stacks: Result<Vec<StackStatus>, String>,
    /// The range the traffic figures cover.
    range: Range,
    /// The traffic history over [`Self::range`], or why it cannot be read.
    history: Result<TrafficHistory, String>,
    /// The requests in flight now to every stack's routes and unrouted, without hoster's own:
    /// the one live traffic figure (ADR-0062 §8).
    inflight: u64,
    /// The deployments of the last [`Stats::DEPLOY_HISTORY`] per stack, or why they cannot be
    /// read.
    deployments: Result<Vec<DeployStats>, String>,
    /// The certificates the proxy serves, for the expiry warnings.
    served: Vec<CertExpiry>,
    certificates: CertGlance,
    /// The newest events, or why they cannot be read.
    events: Result<Vec<LoggedEvent>, String>,
    /// How many updates (majors aside) each stack is offered (ADR-0061 §9); empty when they
    /// cannot be read.
    updates: BTreeMap<String, usize>,
}

impl Readings {
    async fn gather(
        stacks: &Stacks,
        stats: &Stats,
        certificates: &Certificates,
        range: Range,
    ) -> Self {
        let query = RecentEvents::query(None);
        let (status, history, deployments, glance, events, updates) = tokio::join!(
            stacks.status(),
            stats.history(range),
            stats.deployments(),
            certificates.glance(SystemTime::now()),
            stacks.event_page(&query),
            stacks.update_counts(),
        );
        Readings {
            stacks: status.map_err(|err| {
                tracing::warn!(error = %err, "cannot read the stacks for the dashboard");
                format!("Cannot show the stacks: {err}.")
            }),
            range,
            history: history.map_err(|err| {
                tracing::warn!(
                    error = format!("{err:#}"),
                    "cannot read the traffic history"
                );
                format!("Cannot show the traffic history: {err:#}.")
            }),
            inflight: Self::inflight(&stats.traffic()),
            deployments: deployments.map_err(|err| {
                tracing::warn!(
                    error = format!("{err:#}"),
                    "cannot read deployment statistics"
                );
                format!("Cannot show the deployment statistics: {err:#}.")
            }),
            served: stats.certificates(),
            certificates: glance,
            events: events.map(|page| page.events).map_err(|err| {
                tracing::warn!(error = %err, "cannot read the event log for the dashboard");
                format!("Cannot show the recent events: {err}.")
            }),
            updates,
        }
    }

    /// The requests in flight in `traffic` to every stack's routes and unrouted, without
    /// hoster's own routes.
    fn inflight(traffic: &TrafficSnapshot) -> u64 {
        traffic
            .routes
            .iter()
            .filter(|r| !r.is_reserved())
            .map(|r| r.counts.inflight)
            .chain([traffic.unrouted.inflight])
            .sum()
    }
}

/// Nothing at all, over the default range, as of now.
impl Default for Readings {
    fn default() -> Self {
        let range = Range::default();
        Readings {
            stacks: Ok(Vec::new()),
            range,
            history: Ok(TrafficHistory::empty(range, Timestamp::now())),
            inflight: 0,
            deployments: Ok(Vec::new()),
            served: Vec::new(),
            certificates: CertGlance::default(),
            events: Ok(Vec::new()),
            updates: BTreeMap::new(),
        }
    }
}

/// Everything under the dashboard's header: the tiles, the warnings, the charts, the stacks,
/// the certificates and the recent events. The live region's content.
#[derive(Debug, Template)]
#[template(path = "dashboard_overview.html")]
pub struct Overview {
    /// The range the traffic figures cover.
    range: Range,
    kpis: Vec<Kpi>,
    warnings: Vec<Warning>,
    /// Responses by status over the range, for all hosted traffic.
    statuses: StackedBars,
    /// The p50 and p95 response times over the range, for all hosted traffic.
    latency: LineChart,
    stacks: Vec<StackRow>,
    /// The stacks cannot be read, so the empty table does not mean there are none.
    unreadable: bool,
    /// How many stacks are in each condition: `2 running · 1 degraded`.
    summary: String,
    /// The requests no stack row counts.
    other: Vec<OtherTraffic>,
    certificates: CertGlance,
    recent: RecentEvents,
    /// What could not be read.
    problems: Vec<String>,
}

impl Overview {
    /// What the charts and the tiles say they cover.
    const ALL: &str = "all stacks";

    /// The overview of `readings` as of `now`.
    fn new(readings: Readings, now: SystemTime) -> Self {
        let Readings {
            stacks,
            range,
            history,
            inflight,
            deployments,
            served,
            certificates,
            events,
            updates,
        } = readings;
        let mut problems = Vec::new();
        // `None` when the stacks cannot be read: then which of them exist is not known.
        let known = stacks.map_err(|problem| problems.push(problem)).ok();
        let stacks = known.as_deref().unwrap_or_default();
        // A history that cannot be read draws as one without any: empty bars, `-` figures.
        let history = history.unwrap_or_else(|problem| {
            problems.push(problem);
            TrafficHistory::empty(range, Timestamp::from(now))
        });
        let deployments = deployments.map_err(|problem| problems.push(problem)).ok();
        let recent = RecentEvents::new(events.map_err(|problem| problems.push(problem)).ok(), now);
        let rows: Vec<StackRow> = stacks
            .iter()
            .map(|status| {
                let history_of = deployments
                    .as_ref()
                    .map(|all| all.iter().find(|d| d.stack == status.name));
                StackRow::new(status, &history, history_of, now)
            })
            .collect();
        let mut warnings: Vec<Warning> = stacks.iter().flat_map(Warning::of_stack).collect();
        warnings.extend(Warning::of_certificates(stacks, &served, now));
        warnings.extend(Warning::of_updates(&updates));
        let summary = Condition::ALL
            .into_iter()
            .filter_map(|condition| {
                let count = rows.iter().filter(|r| r.condition == condition).count();
                (count > 0).then(|| format!("{count} {condition}"))
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let all = history.all();
        let timeline = Timeline::from(range);
        Overview {
            range,
            kpis: Self::kpis(
                range,
                all,
                inflight,
                known.is_some().then_some(&rows),
                &certificates,
            ),
            warnings,
            statuses: StackedBars::new(timeline, all.bars.iter().map(|b| b.traffic.statuses))
                .of(Self::ALL),
            latency: LineChart::new(
                timeline,
                all.bars
                    .iter()
                    .map(|b| Percentiles::from(&b.traffic.latency)),
            )
            .of(Self::ALL),
            other: OtherTraffic::all(&history, deployments.as_deref(), known.as_deref()),
            unreadable: known.is_none(),
            stacks: rows,
            summary,
            certificates,
            recent,
            problems,
        }
    }

    /// The tiles; `rows` is `None` when the stacks cannot be read. The traffic tiles are about
    /// what hoster hosts over `range` (`all`): they leave out hoster's own routes, which the
    /// note under the stacks table counts. `inflight` is live.
    fn kpis(
        range: Range,
        all: &ScopeHistory,
        inflight: u64,
        rows: Option<&Vec<StackRow>>,
        certificates: &CertGlance,
    ) -> Vec<Kpi> {
        let totals = &all.totals;
        let previous = all.previous.as_ref();
        let line = TrafficLine::from(totals);
        let covers = format!(
            "over the last {range}, to every stack's routes and unrouted, not hoster's own \
             admin UI; the change is against the {range} before it"
        );
        let requests_sub = [
            Some(
                Delta::requests(totals.requests(), previous.map(TrafficTotals::requests))
                    .map_or_else(|| Delta::NONE.to_owned(), |d| d.to_string()),
            ),
            (inflight > 0).then(|| format!("{} in flight", Count(inflight))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let errors_sub = if totals.requests() == 0 {
            "no requests".to_owned()
        } else {
            format!(
                "{} of {}",
                Count(totals.server_errors()),
                Count(totals.requests())
            )
        };
        let latency_sub = |now: Option<Duration>, before: Option<Duration>| match now {
            None => "no requests".to_owned(),
            Some(_) => Delta::latency(now, before)
                .map_or_else(|| Delta::NONE.to_owned(), |d| d.to_string()),
        };
        let p50 = latency_sub(totals.p50(), previous.and_then(TrafficTotals::p50));
        let p95 = latency_sub(totals.p95(), previous.and_then(TrafficTotals::p95));
        vec![
            Kpi::new(
                format!("requests · {range}"),
                Count(totals.requests()),
                requests_sub,
            )
            .title(format!(
                "Finished requests {covers}. In flight: requests under way now"
            )),
            Kpi::new("5xx rate", &line.errors, errors_sub)
                .tone(line.errors_tone)
                .title(format!(
                    "Share of 5xx responses among the finished requests over the last {range}"
                )),
            Kpi::new("p50 latency", &line.p50, p50).title(format!(
                "Median time to the response header of the finished requests {covers}"
            )),
            Kpi::new("p95 latency", &line.p95, p95)
                .tone(line.p95_tone)
                .title(format!(
                    "95th percentile time to the response header of the finished requests \
                     {covers}"
                )),
            StackRow::kpi(rows),
            certificates.kpi(),
        ]
    }

    /// The tone of the warnings' alert: that of the worst warning.
    fn alert_tone(&self) -> Tone {
        if self
            .warnings
            .iter()
            .any(|w| Tone::from(w.kind) == Tone::Fail)
        {
            Tone::Fail
        } else {
            Tone::Warn
        }
    }

    fn alert_icon(&self) -> Icon {
        match self.alert_tone() {
            Tone::Fail => Icon::Failed,
            _ => Icon::Warning,
        }
    }
}

/// How a tile's figure changed against the period of equal length just before the range
/// (ADR-0062 §8), as its sub-line writes it: `▲ 6.2 % vs prev`, `▼ 2.0 ms vs prev`. A change is
/// not good or bad by itself, so it has no tone.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Delta {
    /// A count, by this many percent of what it was.
    Count { direction: Ordering, percent: f64 },
    /// A latency, by this much.
    Latency { direction: Ordering, by: Duration },
}

impl Delta {
    /// The sub-line where there is nothing to compare with.
    const NONE: &str = "no previous data";

    /// The change of a count from `before`; `None` when the period before had nothing.
    fn requests(now: u64, before: Option<u64>) -> Option<Self> {
        let before = before.filter(|before| *before > 0)?;
        Some(Delta::Count {
            direction: now.cmp(&before),
            percent: now.abs_diff(before) as f64 * 100.0 / before as f64,
        })
    }

    /// The change of a latency from `before`; `None` without both.
    fn latency(now: Option<Duration>, before: Option<Duration>) -> Option<Self> {
        let (now, before) = (now?, before?);
        Some(Delta::Latency {
            direction: now.cmp(&before),
            by: now.abs_diff(before),
        })
    }
}

impl fmt::Display for Delta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Delta::Count { direction, .. } | Delta::Latency { direction, .. }) = *self;
        let arrow = match direction {
            Ordering::Greater => "▲",
            Ordering::Less => "▼",
            Ordering::Equal => return f.write_str("unchanged vs prev"),
        };
        match *self {
            Delta::Count { percent, .. } => write!(f, "{arrow} {percent:.1} % vs prev"),
            Delta::Latency { by, .. } => write!(f, "{arrow} {} vs prev", Elapsed(by)),
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

    /// The health, crash-loop and deploy warnings of one stack (certificates are warned about
    /// once per host, by [`Warning::of_certificates`]).
    fn of_stack(status: &StackStatus) -> Vec<Warning> {
        let url = StackView::url(&status.name);
        let mut warnings = Vec::new();
        for service in &status.services {
            let kind = if service.is_crash_looping() {
                WarningKind::CrashLooping
            } else if matches!(service.state, ServiceState::Running { .. })
                && service.health.status == HealthStatus::Unhealthy
            {
                WarningKind::Unhealthy
            } else {
                continue;
            };
            // A crash loop names its last exit first, OOM kills included (ADR-0059 §6).
            let last_exit = (kind == WarningKind::CrashLooping)
                .then_some(service.restarts.last_exit.as_ref())
                .flatten()
                .map(|exit| format!("last: {exit}"));
            let detail = [last_exit, service.health.last.clone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            warnings.push(Warning {
                kind,
                subject: format!("{} ({})", service.name, status.name),
                detail,
                href: url.clone(),
            });
        }
        // While a deployment runs, the failure before it is history.
        if let Some(last) = status
            .last
            .as_ref()
            .filter(|last| status.active.is_none() && Tone::from(last.state) == Tone::Fail)
        {
            let error = last.error.clone().unwrap_or_default();
            warnings.push(Warning {
                kind: WarningKind::DeployFailed,
                subject: status.name.clone(),
                detail: format!("deployment {} {}: {error}", last.id, last.state)
                    .trim_end_matches([':', ' '])
                    .to_owned(),
                href: DeploymentView::url(&status.name, last.id),
            });
        }
        warnings
    }

    /// A warning for every stack offered updates (`updates`, by stack, majors aside).
    fn of_updates(updates: &BTreeMap<String, usize>) -> Vec<Warning> {
        updates
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(stack, count)| Warning {
                kind: WarningKind::UpdatesAvailable,
                subject: stack.clone(),
                detail: format!(
                    "{count} {} available",
                    if *count == 1 { "update" } else { "updates" }
                ),
                href: format!("{}#updates", StackView::url(stack)),
            })
            .collect()
    }

    /// A warning for every `served` certificate expiring soon, naming the services of `stacks`
    /// that route its host.
    fn of_certificates(
        stacks: &[StackStatus],
        served: &[CertExpiry],
        now: SystemTime,
    ) -> Vec<Warning> {
        let mut routed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for status in stacks {
            for (service, hosts) in StackRow::hosts(status).into_iter().flatten() {
                for host in hosts {
                    routed
                        .entry(host)
                        .or_default()
                        .push(format!("{service} ({})", status.name));
                }
            }
        }
        served
            .iter()
            .filter_map(|cert| {
                let left = cert.remaining(now);
                if !CertState::from(left).is_flagged() {
                    return None;
                }
                let left = if left.is_zero() {
                    "expired".to_owned()
                } else {
                    format!("{} left", Elapsed(left))
                };
                // A host no stack routes is hoster's own, a removed stack's, or one whose
                // stacks cannot be read: who uses it is not known here, so nobody is named.
                let detail = match routed.get(&cert.host) {
                    Some(users) => format!("{left}, used by {}", users.join(", ")),
                    None => left,
                };
                Some(Warning {
                    kind: WarningKind::CertExpiring,
                    subject: cert.host.clone(),
                    detail,
                    href: "/certificates".to_owned(),
                })
            })
            .collect()
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
    /// Newer third-party images are on offer (ADR-0061 §9).
    UpdatesAvailable,
}

impl fmt::Display for WarningKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unhealthy => "failing health check",
            Self::CrashLooping => "crash looping",
            Self::CertExpiring => "certificate expiring",
            Self::DeployFailed => "deploy failed",
            Self::UpdatesAvailable => "updates",
        })
    }
}

/// The same tone the condition has in its own cell: a failing probe and a crash loop as in
/// [`HealthStatus`], a failed deploy as [`crate::store::DeploymentState::Failed`].
impl From<WarningKind> for Tone {
    fn from(kind: WarningKind) -> Self {
        match kind {
            WarningKind::Unhealthy | WarningKind::CrashLooping | WarningKind::DeployFailed => {
                Tone::Fail
            }
            WarningKind::CertExpiring => Tone::Warn,
            WarningKind::UpdatesAvailable => Tone::Neutral,
        }
    }
}

/// One stack in the stacks table.
#[derive(Debug)]
struct StackRow {
    name: String,
    url: String,
    history_url: String,
    condition: Condition,
    /// "revision N: deploy failed: …" (ADR-0049).
    flag: Option<String>,
    /// Running services that pass their health check (or have none yet), out of all.
    healthy: usize,
    services: usize,
    /// Each service in words, for the tooltip of the healthy count.
    health_detail: String,
    /// `None` when the stack routes nothing and saw no traffic in the history read.
    traffic: Option<RowTraffic>,
    /// Its active revision does not parse, so which hosts it routes is not known.
    revision_unreadable: bool,
    /// The restarts of its running services' current generation.
    restarts: u64,
    crash_looping: usize,
    /// Each running service's restarts, for the tooltip.
    restarts_detail: String,
    /// Its finished deployments of the last [`Stats::DEPLOY_HISTORY`]; `None` when they could
    /// not be read.
    deploys: Option<DeploysCell>,
    /// The deployment running now, as (id, url, state).
    active: Option<(String, String, Pill)>,
    /// The latest deployment, when none runs.
    last: Option<LastDeploy>,
    queued: Option<String>,
}

impl StackRow {
    fn new(
        status: &StackStatus,
        history: &TrafficHistory,
        deployments: Option<Option<&DeployStats>>,
        now: SystemTime,
    ) -> Self {
        let url = StackView::url(&status.name);
        let hosts = Self::hosts(status);
        let routed = hosts
            .iter()
            .flat_map(BTreeMap::values)
            .any(|hosts| !hosts.is_empty());
        let seen = history.stacks().any(|(name, _)| name == status.name);
        let condition = Condition::from(status);
        let running: Vec<&ServiceStatus> = status
            .services
            .iter()
            .filter(|s| matches!(s.state, ServiceState::Running { .. }))
            .collect();
        StackRow {
            history_url: format!("{url}/deployments"),
            url,
            name: status.name.clone(),
            condition,
            flag: status.flag.clone(),
            healthy: running.iter().filter(|s| !Self::failing(s)).count(),
            services: status.services.len(),
            health_detail: status
                .services
                .iter()
                .map(|s| format!("{}: {}", s.name, Self::health_word(s)))
                .collect::<Vec<_>>()
                .join(", "),
            traffic: (routed || seen)
                .then(|| RowTraffic::new(history.stack(&status.name), condition)),
            revision_unreadable: hosts.is_none(),
            restarts: running.iter().map(|s| u64::from(s.restarts.restarts)).sum(),
            crash_looping: running.iter().filter(|s| s.is_crash_looping()).count(),
            restarts_detail: if running.is_empty() {
                "No service runs".to_owned()
            } else {
                running
                    .iter()
                    .map(|s| format!("{}: {}", s.name, s.restarts.restarts))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            deploys: deployments.map(DeploysCell::from),
            active: status.active.map(|(id, state)| {
                (
                    id.to_string(),
                    DeploymentView::url(&status.name, id),
                    Pill::from(state),
                )
            }),
            last: status
                .last
                .as_ref()
                .filter(|_| status.active.is_none())
                .map(|last| LastDeploy::new(last, now)),
            queued: status.queued.map(|id| id.to_string()),
        }
    }

    /// The hosts each service of the stack's active revision routes. `None` when the revision
    /// does not parse (it did when it was applied, so only a newer hoster could refuse it).
    fn hosts(status: &StackStatus) -> Option<BTreeMap<String, Vec<String>>> {
        let stack = status.revision.toml.parse::<Stack>().ok()?;
        let hosts = stack
            .services
            .iter()
            .map(|(name, service)| {
                let hosts: BTreeSet<String> =
                    service.routes.iter().map(|r| r.host.to_string()).collect();
                (name.to_string(), hosts.into_iter().collect())
            })
            .collect();
        Some(hosts)
    }

    /// A running service whose probe fails, or that keeps crashing (ADR-0028, ADR-0038).
    fn failing(service: &ServiceStatus) -> bool {
        service.is_crash_looping() || service.health.status == HealthStatus::Unhealthy
    }

    /// A service's state in a word or two, for the healthy count's tooltip.
    fn health_word(service: &ServiceStatus) -> String {
        match service.state {
            ServiceState::Running { .. } if service.is_crash_looping() => {
                "crash looping".to_owned()
            }
            ServiceState::Running { .. } => match service.health.status {
                HealthStatus::Unknown => "running, not probed yet".to_owned(),
                status => status.to_string(),
            },
            ServiceState::AwaitingFirstDigest => "awaiting its first digest".to_owned(),
            ServiceState::NotDeployed => "not deployed".to_owned(),
        }
    }

    /// The healthy count's tone: a warning while not every service is healthy.
    fn healthy_tone(&self) -> Tone {
        if self.healthy < self.services {
            Tone::Warn
        } else {
            Tone::Neutral
        }
    }

    fn restarts_tone(&self) -> Tone {
        if self.crash_looping > 0 {
            Tone::Fail
        } else {
            Tone::Neutral
        }
    }

    /// The stacks tile: running stacks out of all, in the tone of the worst condition. `None`
    /// when the stacks cannot be read.
    fn kpi(rows: Option<&Vec<StackRow>>) -> Kpi {
        let title = "Stacks whose services all run and pass their health checks, out of all";
        let Some(rows) = rows else {
            return Kpi::new("stacks", "-", "cannot be read")
                .tone(Tone::Fail)
                .title(title);
        };
        let count = |condition| rows.iter().filter(|r| r.condition == condition).count();
        let (running, degraded, deploying, failed, waiting) = (
            count(Condition::Running),
            count(Condition::Degraded),
            count(Condition::Deploying),
            count(Condition::Failed),
            count(Condition::NotDeployed),
        );
        let mut sub = format!("{degraded} degraded · {deploying} deploying");
        if failed > 0 {
            sub.push_str(&format!(" · {failed} failed"));
        }
        if waiting > 0 {
            sub.push_str(&format!(" · {waiting} not deployed"));
        }
        let tone = if failed > 0 {
            Tone::Fail
        } else if degraded > 0 {
            Tone::Warn
        } else if deploying > 0 {
            Tone::Progress
        } else if running > 0 && running == rows.len() {
            Tone::Ok
        } else {
            Tone::Neutral
        };
        Kpi::new("stacks", format!("{running} / {}", rows.len()), sub)
            .tone(tone)
            .title(title)
    }
}

/// A stack's finished deployments of the last [`Stats::DEPLOY_HISTORY`].
#[derive(Debug, Default)]
struct DeploysCell {
    live: u32,
    failed: u32,
    skipped: u32,
    /// The mean and longest duration of those that went live, `-` without any.
    mean: String,
    max: String,
}

impl DeploysCell {
    /// The cell's tooltip.
    fn title(&self) -> String {
        format!(
            "Finished deployments of the last {}: {} went live, {} failed or were rejected, {} had nothing to do or were superseded; durations of those that went live",
            Elapsed(Stats::DEPLOY_HISTORY),
            self.live,
            self.failed,
            self.skipped
        )
    }
}

/// `None` is a stack without a finished deployment in the history.
impl From<Option<&DeployStats>> for DeploysCell {
    fn from(stats: Option<&DeployStats>) -> Self {
        stats.map_or_else(DeploysCell::default, |stats| DeploysCell {
            live: stats.succeeded(),
            failed: stats.failed(),
            skipped: stats.skipped(),
            mean: Elapsed::cell(stats.mean_live_duration()),
            max: Elapsed::cell(stats.max_live_duration()),
        })
    }
}

/// What no stack row shows over the range: unrouted requests, those to hoster's own routes
/// (the admin UI), and the requests and deployments of a stack hoster no longer has (or of every
/// stack, while the stacks cannot be read).
#[derive(Debug)]
struct OtherTraffic {
    /// `unrouted`, `hoster (admin UI)`, `blog (removed)`, or `blog` while the stacks cannot be
    /// read.
    label: String,
    traffic: TrafficLine,
    /// Requests that failed because of their upstream.
    upstream_errors: u64,
    /// The count of every status class, for the entry's tooltip.
    statuses: String,
    /// The finished deployments of the last [`Stats::DEPLOY_HISTORY`] of a stack no row shows.
    deploys: Option<DeploysCell>,
}

impl OtherTraffic {
    /// The unrouted requests always, then every other owner that saw requests over the range
    /// or, for a stack, deployments. `stacks` is `None` when they cannot be read, and
    /// `deployments` when those cannot.
    fn all(
        history: &TrafficHistory,
        deployments: Option<&[DeployStats]>,
        stacks: Option<&[StackStatus]>,
    ) -> Vec<Self> {
        // The label of a stack no row shows, `None` for one a row shows.
        let stack_label = |stack: &str| match stacks {
            Some(stacks) if stacks.iter().any(|s| s.name == stack) => None,
            Some(_) => Some(format!("{stack} (removed)")),
            None => Some(stack.to_owned()),
        };
        let mut owners: BTreeMap<String, (Option<&TrafficTotals>, Option<&DeployStats>)> =
            BTreeMap::new();
        for (purpose, scope) in history.reserved() {
            owners.entry(format!("hoster ({purpose})")).or_default().0 = Some(&scope.totals);
        }
        for (stack, scope) in history.stacks() {
            if let Some(label) = stack_label(stack) {
                owners.entry(label).or_default().0 = Some(&scope.totals);
            }
        }
        for stats in deployments.unwrap_or_default() {
            if let Some(label) = stack_label(&stats.stack) {
                owners.entry(label).or_default().1 = Some(stats);
            }
        }
        let range = history.range();
        std::iter::once(Self::new(
            range,
            "unrouted".to_owned(),
            Some(&history.unrouted().totals),
            None,
        ))
        .chain(
            owners
                .into_iter()
                .map(|(label, (totals, deploys))| Self::new(range, label, totals, deploys))
                .filter(|other| other.traffic.requests > 0 || other.deploys.is_some()),
        )
        .collect()
    }

    fn new(
        range: Range,
        label: String,
        totals: Option<&TrafficTotals>,
        deploys: Option<&DeployStats>,
    ) -> Self {
        let totals = totals.copied().unwrap_or_default();
        let statuses = StatusClass::ALL
            .into_iter()
            .map(|class| format!("{class} {}", Count(totals.statuses.get(class))))
            .collect::<Vec<_>>()
            .join(" · ");
        OtherTraffic {
            label,
            upstream_errors: totals.upstream_errors,
            statuses: format!("Responses over the last {range}: {statuses}"),
            traffic: TrafficLine::from(&totals),
            deploys: deploys.map(|d| DeploysCell::from(Some(d))),
        }
    }

    fn requests(&self) -> Count {
        Count(self.traffic.requests)
    }

    fn upstream_errors(&self) -> Count {
        Count(self.upstream_errors)
    }
}

#[cfg(test)]
pub(super) mod tests;
