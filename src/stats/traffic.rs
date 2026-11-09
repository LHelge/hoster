//! Per-route request statistics, kept in memory and updated by the proxy (ADR-0045).
//!
//! Each (host, prefix) route gets its counters on its first request; they are atomics, so the
//! proxy's threads update them without locking. The registry of routes is an `ArcSwap`ped map
//! that is copied only when a route is seen for the first time, so finding a route's counters
//! is two hash lookups. Counters of routes that are gone stay until hoster restarts.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use super::latency::{Latency, LatencyWindow};
use crate::routing::{Owner, RouteEntry};

/// The class of a response status, as the statistics count it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatusClass {
    /// No response header went out: the client went away, or the connection failed.
    NoResponse,
    Informational,
    Success,
    Redirection,
    ClientError,
    ServerError,
}

impl StatusClass {
    pub const ALL: [StatusClass; 6] = [
        StatusClass::NoResponse,
        StatusClass::Informational,
        StatusClass::Success,
        StatusClass::Redirection,
        StatusClass::ClientError,
        StatusClass::ServerError,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

impl From<Option<u16>> for StatusClass {
    /// The class of the status sent, if one was. Anything outside 100–599 counts as a server
    /// error.
    fn from(status: Option<u16>) -> Self {
        match status {
            None => StatusClass::NoResponse,
            Some(100..=199) => StatusClass::Informational,
            Some(200..=299) => StatusClass::Success,
            Some(300..=399) => StatusClass::Redirection,
            Some(400..=499) => StatusClass::ClientError,
            Some(_) => StatusClass::ServerError,
        }
    }
}

impl fmt::Display for StatusClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StatusClass::NoResponse => "none",
            StatusClass::Informational => "1xx",
            StatusClass::Success => "2xx",
            StatusClass::Redirection => "3xx",
            StatusClass::ClientError => "4xx",
            StatusClass::ServerError => "5xx",
        })
    }
}

/// How one request ended, as the proxy's `logging` hook reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestOutcome {
    /// The status sent to the client, if any.
    pub status: Option<u16>,
    /// From reading the request header to the response header: how long the client waited for
    /// an answer, not how long the body or stream then lasted. The whole request for one that
    /// never got an upstream response header.
    pub latency: Duration,
    /// The request failed because of its upstream (connect, read or write).
    pub upstream_error: bool,
}

/// The request statistics of every route the proxy served since hoster started. A cheap handle:
/// clones share the counters.
#[derive(Debug, Clone)]
pub struct Traffic(Arc<Registry>);

#[derive(Debug)]
struct Registry {
    /// Latency windows count time from here.
    base: Instant,
    /// Host (normalized) → prefix → counters.
    routes: ArcSwap<HashMap<String, HashMap<String, Arc<RouteCounters>>>>,
    /// Requests that matched no route (unknown hosts, misdirected requests, bad requests).
    unrouted: Arc<RouteCounters>,
}

/// The counters of one route.
#[derive(Debug)]
struct RouteCounters {
    owner: Option<Owner>,
    base: Instant,
    statuses: [AtomicU64; StatusClass::ALL.len()],
    upstream_errors: AtomicU64,
    inflight: AtomicU64,
    latency: LatencyWindow,
}

impl Default for Traffic {
    fn default() -> Self {
        let base = Instant::now();
        Traffic(Arc::new(Registry {
            base,
            routes: ArcSwap::default(),
            unrouted: Arc::new(RouteCounters::new(None, base)),
        }))
    }
}

impl Traffic {
    /// Starts counting a request to `route` on `host` (the route table's spelling of it).
    pub fn route(&self, host: &str, route: &RouteEntry) -> TrackedRequest {
        TrackedRequest::start(self.counters(host, route))
    }

    /// Starts counting a request that matched no route.
    pub fn unrouted(&self) -> TrackedRequest {
        TrackedRequest::start(Arc::clone(&self.0.unrouted))
    }

    /// The counters of `route`, created on its first request. A (host, prefix) that changed
    /// hands (another stack or service routes it now) starts over.
    fn counters(&self, host: &str, route: &RouteEntry) -> Arc<RouteCounters> {
        let current = |routes: &HashMap<String, HashMap<String, Arc<RouteCounters>>>| {
            routes
                .get(host)
                .and_then(|prefixes| prefixes.get(&route.prefix_str))
                .filter(|counters| counters.owner.as_ref() == Some(&route.owner))
                .map(Arc::clone)
        };
        if let Some(counters) = current(&self.0.routes.load()) {
            return counters;
        }
        let fresh = Arc::new(RouteCounters::new(Some(route.owner.clone()), self.0.base));
        let mut found = None;
        self.0.routes.rcu(|routes| {
            if let Some(counters) = current(routes) {
                found = Some(counters);
                return Arc::clone(routes);
            }
            found = None;
            let mut routes = HashMap::clone(routes);
            routes
                .entry(host.to_owned())
                .or_default()
                .insert(route.prefix_str.clone(), Arc::clone(&fresh));
            Arc::new(routes)
        });
        found.unwrap_or(fresh)
    }

    /// The statistics of every route seen so far, sorted by host and prefix, and of the requests
    /// that matched none.
    pub fn snapshot(&self) -> TrafficSnapshot {
        let at = self.0.base.elapsed();
        let mut routes: Vec<RouteTraffic> = self
            .0
            .routes
            .load()
            .iter()
            .flat_map(|(host, prefixes)| {
                prefixes.iter().map(move |(prefix, counters)| RouteTraffic {
                    host: host.clone(),
                    prefix: prefix.clone(),
                    owner: counters.owner.clone(),
                    counts: counters.read(at),
                })
            })
            .collect();
        routes.sort_by(|a, b| (&a.host, &a.prefix).cmp(&(&b.host, &b.prefix)));
        TrafficSnapshot {
            routes,
            unrouted: self.0.unrouted.read(at),
        }
    }
}

impl RouteCounters {
    fn new(owner: Option<Owner>, base: Instant) -> Self {
        Self {
            owner,
            base,
            statuses: Default::default(),
            upstream_errors: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            latency: LatencyWindow::default(),
        }
    }

    fn record(&self, outcome: RequestOutcome) {
        let class = StatusClass::from(outcome.status);
        self.statuses[class.index()].fetch_add(1, Ordering::Relaxed);
        if outcome.upstream_error {
            self.upstream_errors.fetch_add(1, Ordering::Relaxed);
        }
        self.latency.record(self.base.elapsed(), outcome.latency);
    }

    fn read(&self, at: Duration) -> Counts {
        Counts {
            statuses: StatusCounts(
                self.statuses
                    .each_ref()
                    .map(|count| count.load(Ordering::Relaxed)),
            ),
            upstream_errors: self.upstream_errors.load(Ordering::Relaxed),
            inflight: self.inflight.load(Ordering::Relaxed),
            latency: self.latency.summary(at),
        }
    }
}

/// One request being counted: in flight until it is dropped, counted by
/// [`finish`](Self::finish). A request that never finishes (its task was cancelled) leaves the
/// in-flight gauge when dropped, without counting.
#[derive(Debug)]
#[must_use = "the request is only in flight while this is alive"]
pub struct TrackedRequest {
    counters: Arc<RouteCounters>,
    finished: bool,
}

impl TrackedRequest {
    fn start(counters: Arc<RouteCounters>) -> Self {
        counters.inflight.fetch_add(1, Ordering::Relaxed);
        Self {
            counters,
            finished: false,
        }
    }

    /// Counts how the request ended. Only the first call counts.
    pub fn finish(&mut self, outcome: RequestOutcome) {
        if !std::mem::replace(&mut self.finished, true) {
            self.counters.record(outcome);
        }
    }
}

impl Drop for TrackedRequest {
    fn drop(&mut self) {
        self.counters.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Requests by status class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusCounts([u64; StatusClass::ALL.len()]);

impl StatusCounts {
    pub fn get(&self, class: StatusClass) -> u64 {
        self.0[class.index()]
    }

    /// Every finished request.
    pub fn total(&self) -> u64 {
        self.0.iter().sum()
    }

    /// Every class with its count, in [`StatusClass::ALL`] order.
    pub fn iter(&self) -> impl Iterator<Item = (StatusClass, u64)> + '_ {
        StatusClass::ALL
            .into_iter()
            .map(|class| (class, self.get(class)))
    }
}

/// What a set of counters held when read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Finished requests since hoster started.
    pub statuses: StatusCounts,
    /// Requests that failed because of their upstream, since hoster started.
    pub upstream_errors: u64,
    /// Requests in flight now.
    pub inflight: u64,
    /// The latency of the requests of the last few minutes.
    pub latency: Latency,
}

/// The statistics of one route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTraffic {
    pub host: String,
    /// The route's prefix, e.g. `/` or `/api`.
    pub prefix: String,
    /// Who the route belonged to when it was counted.
    pub owner: Option<Owner>,
    pub counts: Counts,
}

impl RouteTraffic {
    /// The stack the route belonged to, unless it is hoster's own.
    pub fn stack(&self) -> Option<&str> {
        match &self.owner {
            Some(Owner::Route(owner)) => Some(owner.stack.as_ref()),
            Some(Owner::Reserved(_)) | None => None,
        }
    }

    /// Whether the route is hoster's own (the admin UI), not a hosted service's.
    pub fn is_reserved(&self) -> bool {
        matches!(self.owner, Some(Owner::Reserved(_)))
    }
}

/// The request statistics at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrafficSnapshot {
    /// Sorted by host and prefix.
    pub routes: Vec<RouteTraffic>,
    /// Requests that matched no route.
    pub unrouted: Counts,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{RouteOwner, Target};
    use crate::stack::RouteTimeouts;

    fn entry(prefix: &str, service: &str) -> RouteEntry {
        RouteEntry {
            owner: Owner::Route(RouteOwner {
                stack: "site".parse().unwrap(),
                service: service.parse().unwrap(),
            }),
            prefix: prefix.parse().unwrap(),
            prefix_str: prefix.to_owned(),
            strip_prefix: false,
            target: Target::Unavailable {
                retry_after: Duration::from_secs(1),
            },
            timeouts: RouteTimeouts::default(),
        }
    }

    fn outcome(status: Option<u16>, ms: u64) -> RequestOutcome {
        RequestOutcome {
            status,
            latency: Duration::from_millis(ms),
            upstream_error: false,
        }
    }

    #[test]
    fn status_classes() {
        assert_eq!(StatusClass::from(None), StatusClass::NoResponse);
        assert_eq!(StatusClass::from(Some(101)), StatusClass::Informational);
        assert_eq!(StatusClass::from(Some(204)), StatusClass::Success);
        assert_eq!(StatusClass::from(Some(308)), StatusClass::Redirection);
        assert_eq!(StatusClass::from(Some(421)), StatusClass::ClientError);
        assert_eq!(StatusClass::from(Some(503)), StatusClass::ServerError);
        assert_eq!(StatusClass::from(Some(999)), StatusClass::ServerError);
        assert_eq!(StatusClass::ClientError.to_string(), "4xx");
    }

    #[test]
    fn requests_are_counted_per_route_by_status_class() {
        let traffic = Traffic::default();
        let root = entry("/", "web");
        let api = entry("/api", "api");
        for status in [200, 201, 404, 502] {
            traffic
                .route("app.example.com", &root)
                .finish(outcome(Some(status), 5));
        }
        traffic
            .route("app.example.com", &api)
            .finish(RequestOutcome {
                upstream_error: true,
                ..outcome(Some(502), 1)
            });
        traffic.unrouted().finish(outcome(Some(404), 0));

        let snapshot = traffic.snapshot();
        let [root, api] = snapshot.routes.as_slice() else {
            panic!("{snapshot:?}");
        };
        assert_eq!(
            (api.host.as_str(), api.prefix.as_str()),
            ("app.example.com", "/api")
        );
        assert_eq!(root.prefix, "/");
        assert_eq!(
            root.owner.as_ref().unwrap().to_string(),
            "service `web` of stack `site`"
        );
        let counts = root.counts.statuses;
        assert_eq!(counts.get(StatusClass::Success), 2);
        assert_eq!(counts.get(StatusClass::ClientError), 1);
        assert_eq!(counts.get(StatusClass::ServerError), 1);
        assert_eq!(counts.total(), 4);
        assert_eq!(root.counts.upstream_errors, 0);
        assert_eq!(root.counts.latency.samples, 4);
        assert_eq!(api.counts.upstream_errors, 1);
        assert_eq!(snapshot.unrouted.statuses.get(StatusClass::ClientError), 1);
    }

    #[test]
    fn in_flight_requests_are_gauged_until_dropped() {
        let traffic = Traffic::default();
        let root = entry("/", "web");
        let mut first = traffic.route("app.example.com", &root);
        let second = traffic.route("app.example.com", &root);
        let gauge = |traffic: &Traffic| traffic.snapshot().routes[0].counts.inflight;
        assert_eq!(gauge(&traffic), 2);
        first.finish(outcome(Some(200), 1));
        first.finish(outcome(Some(200), 1));
        assert_eq!(
            gauge(&traffic),
            2,
            "finishing does not leave the gauge, dropping does"
        );
        drop(first);
        // Dropped without finishing: a cancelled request is not counted.
        drop(second);
        let routes = traffic.snapshot().routes;
        assert_eq!(routes[0].counts.inflight, 0);
        assert_eq!(routes[0].counts.statuses.total(), 1);
    }

    #[test]
    fn a_route_that_changes_hands_starts_over() {
        let traffic = Traffic::default();
        traffic
            .route("app.example.com", &entry("/", "web"))
            .finish(outcome(Some(200), 1));
        traffic
            .route("app.example.com", &entry("/", "web"))
            .finish(outcome(Some(200), 1));
        traffic
            .route("app.example.com", &entry("/", "blog"))
            .finish(outcome(Some(200), 1));
        let routes = traffic.snapshot().routes;
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].counts.statuses.total(), 1);
        assert!(
            routes[0]
                .owner
                .as_ref()
                .unwrap()
                .to_string()
                .contains("blog")
        );
    }

    #[test]
    fn concurrent_requests_share_one_set_of_counters() {
        let traffic = Traffic::default();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let traffic = traffic.clone();
                std::thread::spawn(move || {
                    let root = entry("/", "web");
                    for _ in 0..500 {
                        traffic
                            .route("app.example.com", &root)
                            .finish(outcome(Some(200), 1));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let routes = traffic.snapshot().routes;
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].counts.statuses.get(StatusClass::Success), 2000);
        assert_eq!(routes[0].counts.inflight, 0);
    }
}
