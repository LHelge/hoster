//! The data-driven cards of a stack's overview (epic 8bf, task cu8): the KPI tiles, the slowest
//! routes and the routes of the stack file, from hoster's in-process statistics (ADR-0045) and
//! the active revision.
//!
//! - **KPI tiles**: finished requests since hoster started, p50 and p95 over the latency window,
//!   the share of 5xx responses since start, the restarts of the running services (ADR-0028)
//!   and the deployments that went live or failed over [`Stats::DEPLOY_HISTORY`]. The
//!   percentiles are those of all the stack's requests together, as on the stacks list.
//! - **Slowest routes**: every route of the stack the proxy has counted, by p95 descending, each
//!   with a meter relative to the slowest, its p50, its counts by status class, its upstream
//!   errors and what is in flight.
//! - **Routes**: one row per route of the active revision, `https://host/prefix → service:port`,
//!   tagged with its certificate's time left (or "TLS pending", linked to the certificates page
//!   for the issuance state, while none is served), `strip
//!   prefix` (ADR-0010) and the timeouts it sets (ADR-0011). Routes come from the stack file, so
//!   the card is read-only.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use super::super::list::TrafficLine;
use crate::orchestrator::{ServiceState, StackStatus};
use crate::routing::Owner;
use crate::stack::{Route, Stack};
use crate::stats::{
    CertExpiry, DeployStats, Elapsed, RouteTraffic, Stats, StatusClass, TrafficSnapshot,
};
use crate::web::ui::{CertState, Count, Kpi, Meter, Pill, Tone};

/// What the overview's figures are read from, taken once per render.
#[derive(Debug)]
pub struct Readings {
    pub traffic: TrafficSnapshot,
    /// The certificates served now.
    pub certificates: Vec<CertExpiry>,
    /// Every stack's deployment statistics, or why they cannot be read.
    pub deployments: Result<Vec<DeployStats>, String>,
    pub now: SystemTime,
}

impl Default for Readings {
    fn default() -> Self {
        Self {
            traffic: TrafficSnapshot::default(),
            certificates: Vec::new(),
            deployments: Ok(Vec::new()),
            now: SystemTime::now(),
        }
    }
}

impl Readings {
    /// The statistics as of now. Deployment statistics that cannot be read leave their tile
    /// empty, with the reason as its tooltip.
    pub async fn of(stats: &Stats) -> Self {
        let deployments = stats.deployments().await.map_err(|err| {
            tracing::warn!(
                error = format!("{err:#}"),
                "cannot read deployment statistics"
            );
            format!("{err:#}")
        });
        Self {
            traffic: stats.traffic(),
            certificates: stats.certificates(),
            deployments,
            now: SystemTime::now(),
        }
    }
}

/// The figures of one stack.
#[derive(Debug)]
pub struct Figures {
    pub kpis: Vec<Kpi>,
    pub slowest: Vec<SlowRoute>,
    pub routes: Vec<RouteRow>,
}

impl Figures {
    /// The figures of `status` (whose active revision is `stack`, if it parses) from `readings`.
    pub fn new(status: &StackStatus, stack: Option<&Stack>, readings: &Readings) -> Self {
        let counted: Vec<&RouteTraffic> = readings
            .traffic
            .routes
            .iter()
            .filter(|r| r.stack() == Some(status.name.as_str()))
            .collect();
        let routed =
            stack.is_some_and(|stack| stack.services.values().any(|s| !s.routes.is_empty()));
        let deployments = readings
            .deployments
            .as_ref()
            .map(|all| all.iter().find(|d| d.stack == status.name));
        Figures {
            kpis: Self::kpis(status, &counted, routed, deployments),
            slowest: SlowRoute::all(&counted),
            routes: stack.map_or_else(Vec::new, |stack| {
                RouteRow::all(stack, &readings.certificates, readings.now)
            }),
        }
    }

    fn kpis(
        status: &StackStatus,
        counted: &[&RouteTraffic],
        routed: bool,
        deployments: Result<Option<&DeployStats>, &String>,
    ) -> Vec<Kpi> {
        let traffic = TrafficLine::of(counted.iter().map(|r| &r.counts));
        let window = Elapsed(Stats::LATENCY_WINDOW);
        let latency_sub = if !routed && traffic.routes == 0 {
            "no routes".to_owned()
        } else if traffic.routes > 1 {
            format!("{} routes · {window}", traffic.routes)
        } else {
            format!("last {window}")
        };
        let latency_title = format!(
            "Time to the response header, over the requests to all the stack's routes in the last \
             {window}"
        );
        let requests_sub = match (routed || traffic.routes > 0, traffic.inflight) {
            (false, _) => "no routes".to_owned(),
            (true, 0) => "since hoster started".to_owned(),
            (true, n) => format!("{} in flight · since start", Count(n)),
        };
        let errors_sub = if traffic.requests == 0 {
            "no requests yet".to_owned()
        } else {
            format!(
                "{} of {}",
                Count(traffic.server_errors),
                Count(traffic.requests)
            )
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
        let restarts_kpi = Kpi::new(
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
        );

        let history = Elapsed(Stats::DEPLOY_HISTORY);
        let deploys_kpi = match deployments {
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
        };

        vec![
            Kpi::new("requests", Count(traffic.requests), requests_sub)
                .title("Finished requests to the stack's routes since hoster started"),
            Kpi::new("p50", &traffic.p50, &latency_sub).title(&latency_title),
            Kpi::new("p95", &traffic.p95, &latency_sub)
                .tone(traffic.p95_tone)
                .title(&latency_title),
            Kpi::new("5xx", &traffic.errors, errors_sub)
                .tone(traffic.errors_tone)
                .title("Share of 5xx responses since hoster started"),
            restarts_kpi,
            deploys_kpi,
        ]
    }
}

/// One counted route, as the slowest-routes card shows it.
#[derive(Debug)]
pub struct SlowRoute {
    /// `host` or `host/prefix`.
    pub route: String,
    /// The service it was counted for.
    pub service: String,
    pub p95: String,
    pub p50: String,
    /// The p95 relative to the slowest route's.
    pub meter: Meter,
    /// The requests by status class: 2xx to 5xx always, no-response and 1xx when there were
    /// any.
    pub classes: Vec<(StatusClass, Count)>,
    pub upstream_errors: u64,
    pub inflight: u64,
    /// How many requests the percentiles are taken over.
    pub samples: u64,
}

impl SlowRoute {
    /// The `counted` routes, slowest p95 first; routes without samples last, busiest first.
    fn all(counted: &[&RouteTraffic]) -> Vec<Self> {
        let mut routes: Vec<&RouteTraffic> = counted.to_vec();
        routes.sort_by(|a, b| {
            let key = |r: &RouteTraffic| (r.counts.latency.p95, r.counts.statuses.total());
            key(b).cmp(&key(a))
        });
        let slowest = routes
            .first()
            .and_then(|r| r.counts.latency.p95)
            .unwrap_or_default();
        routes
            .into_iter()
            .map(|route| Self::new(route, slowest))
            .collect()
    }

    fn new(route: &RouteTraffic, slowest: Duration) -> Self {
        let counts = &route.counts;
        let p95 = counts.latency.p95;
        let shown = Elapsed::cell(p95);
        let tone = if p95.is_some_and(|p| p >= TrafficLine::SLOW) {
            Tone::Warn
        } else {
            Tone::Neutral
        };
        let label = match p95 {
            Some(_) if slowest > Duration::ZERO => {
                format!("p95 {shown}, relative to the slowest route")
            }
            _ => "no latency samples".to_owned(),
        };
        SlowRoute {
            route: if route.prefix == "/" {
                route.host.clone()
            } else {
                format!("{}{}", route.host, route.prefix)
            },
            service: match &route.owner {
                Some(Owner::Route(owner)) => owner.service.to_string(),
                _ => String::new(),
            },
            meter: Meter::new(
                p95.unwrap_or_default().as_secs_f64(),
                slowest.as_secs_f64(),
                tone,
                label,
            ),
            p95: shown,
            p50: Elapsed::cell(counts.latency.p50),
            classes: counts
                .statuses
                .iter()
                .filter(|(class, count)| {
                    *count > 0
                        || !matches!(class, StatusClass::NoResponse | StatusClass::Informational)
                })
                .map(|(class, count)| (class, Count(count)))
                .collect(),
            upstream_errors: counts.upstream_errors,
            inflight: counts.inflight,
            samples: counts.latency.samples,
        }
    }

    /// The tooltip of the p50 and p95 figures.
    pub fn latency_title(&self) -> String {
        format!(
            "Time to the response header, over {} requests of the last {}",
            Count(self.samples),
            Elapsed(Stats::LATENCY_WINDOW)
        )
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
    use super::*;
    use crate::web::stacks::tests::{NOTES, notes_status};

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
        assert!(figures.slowest.is_empty() && figures.routes.is_empty());
        let requests = figures.kpis[0].to_string();
        assert!(requests.contains("no routes"), "{requests}");
    }
}
