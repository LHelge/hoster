//! The data-driven cards of a stack's overview (epic 8bf, task cu8; epic t97): the KPI tiles,
//! the responses by status and the routes of the stack file, from the traffic history over the
//! [`TrafficWindow`] (the last hour, ADR-0062 §8), hoster's live statistics and the active
//! revision.
//!
//! - **KPI tiles**: requests per minute over the last [`ScopeHistory::RATE_WINDOW`], with the
//!   busiest minute of the window and how long ago it was (relative, so no time zone is
//!   involved) and the requests in flight now, the one live figure; the p50 and p95 and the
//!   share of 5xx responses (`n of m`) over the window, all the stack's routes together as on
//!   the stacks list; the restarts of the running services (ADR-0028) and the deployments that
//!   went live or failed over [`Stats::DEPLOY_HISTORY`].
//! - **Responses by status**: the stack's requests over the window as stacked bars, one per
//!   bar of the window's plan, with each class's total in the legend.
//! - **Routes**: one row per route of the active revision, `https://host/prefix → service:port`,
//!   tagged with its certificate's time left (or "TLS pending", linked to the certificates page
//!   for the issuance state, while none is served), `strip
//!   prefix` (ADR-0010) and the timeouts it sets (ADR-0011). Routes come from the stack file, so
//!   the card is read-only.
//!
//! The history holds flushed minutes only, so the figures lag by up to a minute. It has no
//! per-route figures (ADR-0062 §8), so the overview shows none: the slowest-routes card, which
//! counted since hoster started, is gone (a slowest-routes panel over history is a proposal,
//! ADR-0062 §10).

use std::collections::BTreeMap;
use std::time::SystemTime;

use super::super::list::{Ago, TrafficLine, TrafficWindow};
use crate::orchestrator::{ServiceState, StackStatus};
use crate::stack::{Route, Stack};
use crate::stats::{
    CertExpiry, DeployStats, Elapsed, InFlight, ResourceUsage, ScopeHistory, Stats, TrafficHistory,
};
use crate::store::Timestamp;
use crate::web::charts::{StackedBars, Timeline};
use crate::web::ui::{CertState, Count, Kpi, PerMinute, Pill, Tone};

/// What the overview's figures are read from, taken once per render.
#[derive(Debug)]
pub struct Readings {
    /// The traffic history over [`TrafficWindow::RANGE`], or why it cannot be read.
    pub history: Result<TrafficHistory, String>,
    /// The requests in flight now, the one live figure.
    pub in_flight: InFlight,
    /// The certificates served now.
    pub certificates: Vec<CertExpiry>,
    /// Every stack's deployment statistics, or why they cannot be read.
    pub deployments: Result<Vec<DeployStats>, String>,
    /// The resource sampler's cache, for the services' CPU and memory (ADR-0066).
    pub resources: ResourceUsage,
    pub now: SystemTime,
}

impl Default for Readings {
    fn default() -> Self {
        let now = SystemTime::now();
        Self {
            history: Ok(TrafficHistory::empty(
                TrafficWindow::RANGE,
                Timestamp::from(now),
            )),
            in_flight: InFlight::default(),
            certificates: Vec::new(),
            deployments: Ok(Vec::new()),
            resources: ResourceUsage::default(),
            now,
        }
    }
}

impl Readings {
    /// The statistics as of now. Statistics that cannot be read leave their tiles empty, with
    /// the reason as their tooltip.
    pub async fn of(stats: &Stats) -> Self {
        let (history, deployments) =
            tokio::join!(stats.history(TrafficWindow::RANGE), stats.deployments());
        let problem = |what: &str, err: anyhow::Error| {
            tracing::warn!(error = format!("{err:#}"), "cannot read {what}");
            format!("{err:#}")
        };
        Self {
            history: history.map_err(|err| problem("the traffic history", err)),
            in_flight: stats.in_flight(),
            certificates: stats.certificates(),
            deployments: deployments.map_err(|err| problem("deployment statistics", err)),
            resources: stats.resources().clone(),
            now: SystemTime::now(),
        }
    }
}

/// The figures of one stack.
#[derive(Debug)]
pub struct Figures {
    pub kpis: Vec<Kpi>,
    pub statuses: StatusChart,
    pub routes: Vec<RouteRow>,
}

/// The overview's responses-by-status card.
#[derive(Debug)]
pub enum StatusChart {
    /// The stack routes nothing and saw no requests in the window.
    NoRoutes,
    /// The traffic history cannot be read, for this reason.
    Unreadable(String),
    Shown(StackedBars),
}

impl Figures {
    /// The figures of `status` (whose active revision is `stack`, if it parses) from `readings`.
    pub fn new(status: &StackStatus, stack: Option<&Stack>, readings: &Readings) -> Self {
        let routed =
            stack.is_some_and(|stack| stack.services.values().any(|s| !s.routes.is_empty()));
        let history = readings.history.as_ref();
        // A stack that routes nothing now still shows the requests it had in the window.
        let shown = history.map(|history| {
            let seen = history.stacks().any(|(name, _)| name == status.name);
            (routed || seen).then(|| (history, history.stack(&status.name)))
        });
        let deployments = readings
            .deployments
            .as_ref()
            .map(|all| all.iter().find(|d| d.stack == status.name));
        let inflight = readings.in_flight.stack(&status.name);
        Figures {
            kpis: Self::kpis(status, shown, inflight, deployments),
            statuses: match shown {
                Err(problem) => StatusChart::Unreadable(problem.clone()),
                Ok(None) => StatusChart::NoRoutes,
                Ok(Some((history, scope))) => StatusChart::Shown(
                    StackedBars::new(
                        Timeline::from(history.range()),
                        scope.bars.iter().map(|b| b.traffic.statuses),
                    )
                    .of(&status.name),
                ),
            },
            routes: stack.map_or_else(Vec::new, |stack| {
                RouteRow::all(stack, &readings.certificates, readings.now)
            }),
        }
    }

    /// The tiles. `traffic` is the stack's history with the scope of the stack, `None` when it
    /// routes nothing and saw no requests, or why the history cannot be read; `inflight` is
    /// live.
    fn kpis(
        status: &StackStatus,
        traffic: Result<Option<(&TrafficHistory, &ScopeHistory)>, &String>,
        inflight: u64,
        deployments: Result<Option<&DeployStats>, &String>,
    ) -> Vec<Kpi> {
        let window = TrafficWindow;
        let mut tiles = match traffic {
            Ok(traffic) => Self::traffic_kpis(traffic, inflight),
            Err(problem) => ["req/min", "p50", "p95", "5xx rate"]
                .into_iter()
                .map(|label| {
                    Kpi::new(format!("{label} · {window}"), "-", "cannot be read")
                        .title(format!("Cannot read the traffic history: {problem}"))
                })
                .collect(),
        };

        let running = status
            .services
            .iter()
            .filter(|s| matches!(s.state, ServiceState::Running { .. }));
        let (restarts, crash_looping, services) =
            running.fold((0u64, 0usize, 0usize), |(restarts, looping, count), s| {
                (
                    restarts + u64::from(s.restarts.restarts),
                    looping + usize::from(s.is_crash_looping()),
                    count + 1,
                )
            });
        tiles.push(
            Kpi::new(
                "restarts",
                Count(restarts),
                if crash_looping > 0 {
                    format!("{crash_looping} crash looping")
                } else if services == 0 {
                    "no running service".to_owned()
                } else if services == 1 {
                    "1 running service".to_owned()
                } else {
                    format!("across {services} running services")
                },
            )
            .tone(if crash_looping > 0 {
                Tone::Fail
            } else {
                Tone::Neutral
            })
            .title(
                "Restarts of the running containers' current generation, since their last deploy or hoster's start (ADR-0028)",
            ),
        );

        let history = Elapsed(Stats::DEPLOY_HISTORY);
        tiles.push(match deployments {
            Ok(stats) => {
                let (live, failed, skipped) =
                    stats.map_or((0, 0, 0), |d| (d.succeeded(), d.failed(), d.skipped()));
                let mut sub = format!("{failed} failed");
                if skipped > 0 {
                    sub.push_str(&format!(" · {skipped} skipped"));
                }
                Kpi::new(format!("deploys · {history}"), format!("{live} live"), sub)
                    .tone(if failed > 0 { Tone::Warn } else { Tone::Neutral })
                    .title(format!(
                        "Finished deployments of the last {history}: live (with or without warnings), failed or rejected, and skipped (nothing to do, or superseded)"
                    ))
            }
            Err(problem) => Kpi::new(format!("deploys · {history}"), "-", "cannot be read")
                .title(format!("Cannot read the deployment statistics: {problem}")),
        });
        tiles
    }

    /// The req/min, p50, p95 and 5xx tiles over the window; `traffic` is `None` when the stack
    /// routes nothing and saw no requests.
    fn traffic_kpis(traffic: Option<(&TrafficHistory, &ScopeHistory)>, inflight: u64) -> Vec<Kpi> {
        let window = TrafficWindow;
        let rate = Elapsed(ScopeHistory::RATE_WINDOW);
        let Some((history, scope)) = traffic else {
            return [
                ("req/min", "0"),
                ("p50", "-"),
                ("p95", "-"),
                ("5xx rate", "-"),
            ]
            .into_iter()
            .map(|(label, value)| {
                Kpi::new(format!("{label} · {window}"), value, "no routes")
                    .title("The stack routes nothing, so there is no traffic to count")
            })
            .collect();
        };
        let totals = &scope.totals;
        let line = TrafficLine::from(totals);
        let peak = scope.peak.map(|peak| {
            format!(
                "peak {}, {}",
                Count(peak.requests),
                Ago::since(peak.at, SystemTime::from(history.end()))
            )
        });
        let rate_sub = [
            Some(peak.unwrap_or_else(|| "no requests".to_owned())),
            (inflight > 0).then(|| format!("{} in flight", Count(inflight))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let latency_sub = if totals.requests() == 0 {
            "no requests".to_owned()
        } else {
            format!(
                "of {} {}",
                Count(totals.requests()),
                if totals.requests() == 1 {
                    "request"
                } else {
                    "requests"
                }
            )
        };
        let errors_sub = if totals.requests() == 0 {
            "no requests".to_owned()
        } else {
            format!(
                "{} of {}",
                Count(totals.server_errors()),
                Count(totals.requests())
            )
        };
        let latency_title = |which: &str| {
            format!(
                "{which} time to the response header over the {window}, all the stack's routes \
                 together"
            )
        };
        vec![
            Kpi::new(
                format!("req/min · {window}"),
                PerMinute(scope.per_minute()),
                rate_sub,
            )
            .title(format!(
                "Requests per minute over the last {rate}; peak: the busiest minute of the \
                 {window}. In flight: requests under way now"
            )),
            Kpi::new(format!("p50 · {window}"), &line.p50, &latency_sub)
                .title(latency_title("Median")),
            Kpi::new(format!("p95 · {window}"), &line.p95, &latency_sub)
                .tone(line.p95_tone)
                .title(latency_title("95th percentile")),
            Kpi::new(format!("5xx rate · {window}"), &line.errors, errors_sub)
                .tone(line.errors_tone)
                .title(format!("Share of 5xx responses over the {window}")),
        ]
    }
}

/// One route of the active revision, as the routes card shows it.
#[derive(Debug, PartialEq, Eq)]
pub struct RouteRow {
    /// `https://host` or `https://host/prefix`.
    pub url: String,
    /// `service:port`.
    pub target: String,
    /// The certificate's time left, plain while it is valid for long.
    pub tls: String,
    /// The same as a pill when the certificate needs a look or none is served yet.
    pub tls_pill: Option<Pill>,
    /// Until when the certificate is valid, or that none is served.
    pub tls_title: String,
    /// `strip prefix`, and the timeouts that are not hoster's default.
    pub tags: Vec<String>,
}

impl RouteRow {
    /// Every route of `stack`, by host and prefix, with the `certificates` served at `now`.
    fn all(stack: &Stack, certificates: &[CertExpiry], now: SystemTime) -> Vec<Self> {
        let served: BTreeMap<&str, &CertExpiry> = certificates
            .iter()
            .map(|cert| (cert.host.as_str(), cert))
            .collect();
        let mut routes: Vec<(&Route, String)> = stack
            .services
            .iter()
            .flat_map(|(service, s)| s.routes.iter().map(move |r| (r, service.to_string())))
            .collect();
        routes.sort_by(|(a, _), (b, _)| {
            (a.host.to_string(), a.prefix.to_string())
                .cmp(&(b.host.to_string(), b.prefix.to_string()))
        });
        routes
            .into_iter()
            .map(|(route, service)| {
                let host = route.host.to_string();
                Self::new(route, &service, served.get(host.as_str()).copied(), now)
            })
            .collect()
    }

    fn new(route: &Route, service: &str, cert: Option<&CertExpiry>, now: SystemTime) -> Self {
        let (tls, state) = match cert.map(|cert| cert.remaining(now)) {
            None => ("TLS pending".to_owned(), CertState::Pending),
            Some(left) if left.is_zero() => ("TLS expired".to_owned(), CertState::Expired),
            Some(left) => {
                let days = left.as_secs() / 86_400;
                let left_text = if days > 0 {
                    format!("{days} d")
                } else {
                    Elapsed(left).to_string()
                };
                (format!("TLS · {left_text} left"), CertState::from(left))
            }
        };
        let mut tags = Vec::new();
        if route.strip_prefix && !route.prefix.is_root() {
            tags.push("strip prefix".to_owned());
        }
        let timeouts = route.timeouts;
        for (kind, timeout) in [
            ("connect", timeouts.connect),
            ("read", timeouts.read),
            ("write", timeouts.write),
            ("idle", timeouts.idle),
        ] {
            if let Some(timeout) = timeout {
                tags.push(format!("{kind} timeout {}", Elapsed(timeout)));
            }
        }
        let title = cert.map_or_else(
            || "No certificate is served for this host yet; the certificates page shows how issuance is going".to_owned(),
            |cert| {
                format!(
                    "Valid until {}",
                    humantime::format_rfc3339_seconds(cert.not_after)
                )
            },
        );
        RouteRow {
            url: if route.prefix.is_root() {
                format!("https://{}", route.host)
            } else {
                format!("https://{}{}", route.host, route.prefix)
            },
            target: format!("{service}:{}", route.port),
            tls_pill: state.pill(&tls).map(|pill| pill.title(&title)),
            tls,
            tls_title: title,
            tags,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::web::dashboard::tests::{MINUTE, bucket, history, hour};
    use crate::web::stacks::tests::{NOTES, notes_status};

    /// The tile labelled `label`.
    fn tile(figures: &Figures, label: &str) -> String {
        let label = format!(r#"<dt class="kpi-label">{label}</dt>"#);
        figures
            .kpis
            .iter()
            .map(ToString::to_string)
            .find(|kpi| kpi.contains(&label))
            .unwrap_or_else(|| panic!("no tile {label}: {:?}", figures.kpis))
    }

    /// The figures of `notes` (which routes `notes.example.com`) from `readings`.
    fn notes_figures(readings: &Readings) -> Figures {
        let status = notes_status();
        let stack: Stack = status.revision.toml.parse().unwrap();
        Figures::new(&status, Some(&stack), readings)
    }

    #[tokio::test]
    async fn the_tiles_and_the_chart_show_the_last_hour_with_its_peak_minute() {
        let notes = ("notes.example.com", "notes", "api");
        let files = ("files.example.com", "notes", "storage");
        let blog = ("blog.example.com", "blog", "web");
        // The newest flushed minute is `hour + 29 min`; the window ends at `hour + 30 min`.
        let history = history(
            TrafficWindow::RANGE,
            vec![
                bucket(hour() - MINUTE * 50, notes, &[(200, 5_000, 5)]),
                bucket(hour() + MINUTE * 18, notes, &[(200, 1_015, 5), (503, 5, 5)]),
                bucket(hour() + MINUTE * 27, notes, &[(200, 30, 5), (404, 10, 5)]),
                bucket(hour() + MINUTE * 29, files, &[(200, 20, 1_500)]),
                bucket(hour() + MINUTE * 29, blog, &[(500, 3_000, 5)]),
            ],
        )
        .await;
        let figures = notes_figures(&Readings {
            history: Ok(history),
            ..Readings::default()
        });
        // 60 requests in the last 5 minutes; the busiest minute 12 minutes before the end.
        let rate = tile(&figures, "req/min · last hour");
        assert!(
            rate.contains(
                r#"<dd class="kpi-value">12</dd><dd class="kpi-sub">peak 1,020, 12 min ago</dd>"#
            ),
            "{rate}"
        );
        let errors = tile(&figures, "5xx rate · last hour");
        assert!(
            errors
                .contains(r#"<dd class="kpi-value">0.5 %</dd><dd class="kpi-sub">5 of 1,080</dd>"#),
            "{errors}"
        );
        assert!(
            tile(&figures, "p50 · last hour").contains("of 1,080 requests"),
            "{:?}",
            figures.kpis
        );
        assert!(
            tile(&figures, "p95 · last hour").contains(r#"<dd class="kpi-value">5"#),
            "{:?}",
            figures.kpis
        );
        let StatusChart::Shown(chart) = &figures.statuses else {
            panic!("{:?}", figures.statuses);
        };
        let legend = chart.legend().to_string();
        assert!(legend.contains("2xx 1,065"), "{legend}");
        assert!(legend.contains("4xx 10"), "{legend}");
        assert!(legend.contains("5xx 5"), "{legend}");
        let svg = chart.to_string();
        assert!(svg.contains("for notes over the last 1h"), "{svg}");
        assert_eq!(svg.matches(r#"class="chart-bar""#).count(), 60, "{svg}");
    }

    #[tokio::test]
    async fn a_quiet_hour_shows_zero_and_dashes_and_the_in_flight_requests() {
        use crate::routing::{Owner, RouteEntry, RouteOwner, Target};
        use crate::stack::RouteTimeouts;
        let traffic = crate::stats::Traffic::default();
        let route = RouteEntry {
            owner: Owner::Route(RouteOwner {
                stack: "notes".parse().unwrap(),
                service: "api".parse().unwrap(),
            }),
            prefix: "/".parse().unwrap(),
            prefix_str: "/".into(),
            strip_prefix: false,
            target: Target::Unavailable {
                retry_after: Duration::from_secs(1),
            },
            timeouts: RouteTimeouts::default(),
        };
        let _open = traffic.route("notes.example.com", &route);
        let figures = notes_figures(&Readings {
            history: Ok(history(TrafficWindow::RANGE, Vec::new()).await),
            in_flight: traffic.in_flight(),
            ..Readings::default()
        });
        let rate = tile(&figures, "req/min · last hour");
        assert!(
            rate.contains(
                r#"<dd class="kpi-value">0</dd><dd class="kpi-sub">no requests · 1 in flight</dd>"#
            ),
            "{rate}"
        );
        for label in ["p50 · last hour", "p95 · last hour", "5xx rate · last hour"] {
            let kpi = tile(&figures, label);
            assert!(
                kpi.contains(r#"<dd class="kpi-value">-</dd><dd class="kpi-sub">no requests</dd>"#),
                "{kpi}"
            );
        }
        let StatusChart::Shown(chart) = &figures.statuses else {
            panic!("{:?}", figures.statuses);
        };
        assert!(chart.to_string().contains("no requests"));
    }

    #[test]
    fn a_history_that_cannot_be_read_leaves_the_traffic_tiles_and_chart_empty() {
        let figures = notes_figures(&Readings {
            history: Err("database is locked".into()),
            ..Readings::default()
        });
        let p95 = tile(&figures, "p95 · last hour");
        assert!(
            p95.contains(r#"<dd class="kpi-value">-</dd><dd class="kpi-sub">cannot be read</dd>"#),
            "{p95}"
        );
        assert!(p95.contains("database is locked"), "{p95}");
        assert!(
            matches!(&figures.statuses, StatusChart::Unreadable(problem) if problem == "database is locked")
        );
        // The other tiles are still there.
        assert_eq!(figures.kpis.len(), 6);
    }

    #[test]
    fn routes_are_listed_with_their_certificate_and_settings() {
        let toml = format!(
            "{NOTES}\n[[services.storage.routes]]\nhost = \"files.example.com\"\nprefix = \"/s3\"\nstrip_prefix = true\nport = 9000\ntimeouts = {{ connect = \"5s\", idle = \"2m\" }}\n"
        );
        let stack: Stack = toml.parse().unwrap();
        let now = SystemTime::now();
        let certificates = [CertExpiry {
            host: "notes.example.com".into(),
            not_after: now + Duration::from_secs(3 * 86_400 + 30),
        }];
        let rows = RouteRow::all(&stack, &certificates, now);
        let [files, notes] = rows.as_slice() else {
            panic!("{rows:?}");
        };
        assert_eq!(files.url, "https://files.example.com/s3");
        assert_eq!(files.target, "storage:9000");
        assert_eq!(files.tls, "TLS pending");
        assert!(files.tls_pill.is_some());
        assert_eq!(
            files.tags,
            [
                "strip prefix",
                "connect timeout 5.0 s",
                "idle timeout 2 min"
            ]
        );
        assert_eq!(notes.url, "https://notes.example.com");
        assert_eq!(notes.tls, "TLS · 3 d left");
        assert!(notes.tls_pill.is_some(), "expiring soon is flagged");
        assert!(notes.tags.is_empty());
    }

    #[test]
    fn a_service_liveness_reports_crash_looping_turns_the_restarts_tile_red() {
        let mut status = notes_status();
        status.services[0].health.status = crate::health::HealthStatus::CrashLooping;
        let figures = Figures::new(&status, None, &Readings::default());
        let restarts = figures
            .kpis
            .iter()
            .map(ToString::to_string)
            .find(|kpi| kpi.contains("restarts"))
            .unwrap();
        assert!(restarts.contains("1 crash looping"), "{restarts}");
        assert!(restarts.contains("fail"), "{restarts}");
    }

    #[test]
    fn deployment_statistics_that_cannot_be_read_leave_their_tile_empty() {
        let readings = Readings {
            deployments: Err("database is locked".into()),
            ..Readings::default()
        };
        let figures = Figures::new(&notes_status(), None, &readings);
        let deploys = figures.kpis.last().unwrap().to_string();
        assert!(
            deploys.contains(r#"<dd class="kpi-value">-</dd>"#),
            "{deploys}"
        );
        assert!(deploys.contains("database is locked"), "{deploys}");
        assert!(figures.routes.is_empty());
        assert!(matches!(figures.statuses, StatusChart::NoRoutes));
        let rate = figures.kpis[0].to_string();
        assert!(rate.contains("no routes"), "{rate}");
    }
}
