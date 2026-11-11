//! Per-route request counters, kept in memory and updated by the proxy (ADR-0045, ADR-0062).
//!
//! Each (host, prefix) route gets its counters on its first request; they are atomics, so the
//! proxy's threads update them without locking. The registry of routes is an `ArcSwap`ped map
//! that is copied only when a route is seen for the first time, so finding a route's counters
//! is two hash lookups.
//!
//! Nothing reads the counters but two things: the traffic recorder, which drains them into the
//! traffic history once a minute, and the in-flight gauge ([`Traffic::in_flight`]), the one
//! live traffic figure the pages show. The counters are cumulative and never reset. [`Traffic::drain`] takes what each route counted
//! since the previous drain (for the traffic history, ADR-0062) by diffing them against the
//! values it saw last, which it keeps to itself, so the proxy's hot path never waits on it.
//! Routes that were idle across a drain leave the registry; their next request starts them
//! over, so the registry holds only routes in use.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::ops::AddAssign;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arc_swap::ArcSwap;

use super::latency::{AtomicHistogram, Histogram};
use crate::routing::{Owner, RouteEntry};
use crate::store::TrafficCounts;

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

/// The request counters of every route the proxy serves. A cheap handle: clones share the
/// counters.
#[derive(Debug, Clone)]
pub struct Traffic(Arc<Registry>);

type Routes = HashMap<String, HashMap<String, Vec<Arc<RouteCounters>>>>;

#[derive(Debug)]
struct Registry {
    /// Host (normalized) → prefix → counters, the current owner's last. Counters of earlier
    /// owners stay until a drain has taken their last requests.
    routes: ArcSwap<Routes>,
    /// Requests that matched no route (unknown hosts, misdirected requests, bad requests).
    unrouted: Arc<RouteCounters>,
    /// What the previous [`Traffic::drain`] saw. Only draining locks it.
    drained: Mutex<Drained>,
}

/// The counters [`Traffic::drain`] follows, with the totals it saw last.
#[derive(Debug, Default)]
struct Drained {
    /// By the address of their counters, which identifies a series: a route that changed
    /// hands has new counters.
    routes: HashMap<usize, Series>,
    unrouted: Totals,
}

/// One route's counters, as the drainer follows them.
#[derive(Debug)]
struct Series {
    host: String,
    prefix: String,
    counters: Arc<RouteCounters>,
    /// The totals at the previous drain.
    last: Totals,
}

/// A route's cumulative counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    statuses: StatusCounts,
    upstream_errors: u64,
    latency: Histogram,
}

/// The counters of one route.
#[derive(Debug)]
struct RouteCounters {
    owner: Option<Owner>,
    statuses: [AtomicU64; StatusClass::ALL.len()],
    upstream_errors: AtomicU64,
    inflight: AtomicU64,
    /// Every latency since the counters started, for [`Traffic::drain`].
    histogram: AtomicHistogram,
}

impl Default for Traffic {
    fn default() -> Self {
        Traffic(Arc::new(Registry {
            routes: ArcSwap::default(),
            unrouted: Arc::new(RouteCounters::new(None)),
            drained: Mutex::default(),
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
        let current = |routes: &Routes| {
            routes
                .get(host)
                .and_then(|prefixes| prefixes.get(&route.prefix_str))
                .and_then(|series| series.last())
                .filter(|counters| counters.owner.as_ref() == Some(&route.owner))
                .map(Arc::clone)
        };
        if let Some(counters) = current(&self.0.routes.load()) {
            return counters;
        }
        let fresh = Arc::new(RouteCounters::new(Some(route.owner.clone())));
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
                .entry(route.prefix_str.clone())
                .or_default()
                .push(Arc::clone(&fresh));
            Arc::new(routes)
        });
        found.unwrap_or(fresh)
    }

    /// What every route counted since the previous drain (or since hoster started), one delta
    /// per series with requests, sorted by host and prefix, then the requests that matched no
    /// route (with an empty host and prefix and no owner), if there were any. A route that
    /// changed hands since is two series, one per owner.
    ///
    /// Requests finishing during a drain count in this one or the next, never in both or
    /// neither. Counters idle across the drain (no request since the previous one, none in
    /// flight) leave the registry; the drainer follows them until nothing else holds them, so
    /// a request that found them just before still counts.
    pub fn drain(&self) -> Vec<TrafficDelta> {
        let mut drained = self
            .0
            .drained
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let registered = self.0.routes.load_full();
        let mut current = HashSet::new();
        for (host, prefixes) in registered.iter() {
            for (prefix, counters) in prefixes
                .iter()
                .flat_map(|(prefix, series)| series.iter().map(move |c| (prefix, c)))
            {
                let key = Arc::as_ptr(counters) as usize;
                current.insert(key);
                drained.routes.entry(key).or_insert_with(|| Series {
                    host: host.clone(),
                    prefix: prefix.clone(),
                    counters: Arc::clone(counters),
                    last: Totals::default(),
                });
            }
        }

        let mut deltas = Vec::new();
        let mut idle = Vec::new();
        drained.routes.retain(|key, series| {
            // Checked before the totals are read: counters nothing else holds (out of the
            // registry, no request on them, no proxy thread about to start one) are final.
            let unreachable = Arc::strong_count(&series.counters) == 1;
            let totals = series.counters.totals();
            let delta = totals.since(&series.last);
            series.last = totals;
            let quiet = delta.is_empty() && series.counters.inflight.load(Ordering::Relaxed) == 0;
            if !delta.is_empty() {
                deltas.push(TrafficDelta {
                    host: series.host.clone(),
                    prefix: series.prefix.clone(),
                    owner: series.counters.owner.clone(),
                    ..TrafficDelta::from(delta)
                });
            }
            if quiet && current.contains(key) {
                // Leaves the registry; still followed while a proxy thread holds it.
                idle.push(Arc::clone(&series.counters));
            }
            !unreachable
        });
        self.forget(&idle);
        deltas.sort_by(|a, b| (&a.host, &a.prefix).cmp(&(&b.host, &b.prefix)));

        let totals = self.0.unrouted.totals();
        let delta = totals.since(&drained.unrouted);
        drained.unrouted = totals;
        if !delta.is_empty() {
            deltas.push(TrafficDelta::from(delta));
        }
        deltas
    }

    /// Takes `idle` counters out of the registry. A route's newer counters, made since, stay.
    fn forget(&self, idle: &[Arc<RouteCounters>]) {
        if idle.is_empty() {
            return;
        }
        self.0.routes.rcu(|routes| {
            let mut routes = HashMap::clone(routes);
            for prefixes in routes.values_mut() {
                for series in prefixes.values_mut() {
                    series.retain(|counters| !idle.iter().any(|gone| Arc::ptr_eq(gone, counters)));
                }
                prefixes.retain(|_, series| !series.is_empty());
            }
            routes.retain(|_, prefixes| !prefixes.is_empty());
            Arc::new(routes)
        });
    }

    /// The requests in flight now, by the stack their route belongs to. Every set of counters
    /// in the registry counts, an earlier owner's too, while it still has requests open.
    pub fn in_flight(&self) -> InFlight {
        let mut stacks = BTreeMap::new();
        for counters in self
            .0
            .routes
            .load()
            .values()
            .flat_map(HashMap::values)
            .flatten()
        {
            let open = counters.inflight.load(Ordering::Relaxed);
            if let (Some(Owner::Route(owner)), 1..) = (&counters.owner, open) {
                *stacks.entry(owner.stack.to_string()).or_default() += open;
            }
        }
        InFlight {
            stacks,
            unrouted: self.0.unrouted.inflight.load(Ordering::Relaxed),
        }
    }
}

impl RouteCounters {
    fn new(owner: Option<Owner>) -> Self {
        Self {
            owner,
            statuses: Default::default(),
            upstream_errors: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            histogram: AtomicHistogram::default(),
        }
    }

    fn record(&self, outcome: RequestOutcome) {
        let class = StatusClass::from(outcome.status);
        self.statuses[class.index()].fetch_add(1, Ordering::Relaxed);
        if outcome.upstream_error {
            self.upstream_errors.fetch_add(1, Ordering::Relaxed);
        }
        self.histogram.record(outcome.latency);
    }

    fn totals(&self) -> Totals {
        Totals {
            statuses: self.statuses(),
            upstream_errors: self.upstream_errors.load(Ordering::Relaxed),
            latency: self.histogram.load(),
        }
    }

    fn statuses(&self) -> StatusCounts {
        StatusCounts(
            self.statuses
                .each_ref()
                .map(|count| count.load(Ordering::Relaxed)),
        )
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
}

impl AddAssign for StatusCounts {
    fn add_assign(&mut self, other: StatusCounts) {
        for (sum, count) in self.0.iter_mut().zip(other.0) {
            *sum = sum.saturating_add(count);
        }
    }
}

/// The status classes of a traffic history bucket.
impl From<&TrafficCounts> for StatusCounts {
    fn from(counts: &TrafficCounts) -> Self {
        let mut statuses = StatusCounts::default();
        for (class, count) in [
            (StatusClass::NoResponse, counts.none),
            (StatusClass::Informational, counts.s1xx),
            (StatusClass::Success, counts.s2xx),
            (StatusClass::Redirection, counts.s3xx),
            (StatusClass::ClientError, counts.s4xx),
            (StatusClass::ServerError, counts.s5xx),
        ] {
            statuses.0[class.index()] = count;
        }
        statuses
    }
}

/// Counts from `(class, count)` pairs; a class named twice adds up, one left out is zero.
impl FromIterator<(StatusClass, u64)> for StatusCounts {
    fn from_iter<I: IntoIterator<Item = (StatusClass, u64)>>(pairs: I) -> Self {
        let mut counts = StatusCounts::default();
        for (class, count) in pairs {
            let sum = &mut counts.0[class.index()];
            *sum = sum.saturating_add(count);
        }
        counts
    }
}

impl StatusCounts {
    /// The requests counted here but not in `earlier`, an earlier reading of the same counters.
    fn since(&self, earlier: &StatusCounts) -> StatusCounts {
        StatusCounts(std::array::from_fn(|i| {
            self.0[i].wrapping_sub(earlier.0[i])
        }))
    }
}

impl Totals {
    /// What was counted between `earlier` and these totals of the same counters.
    fn since(&self, earlier: &Totals) -> Totals {
        Totals {
            statuses: self.statuses.since(&earlier.statuses),
            upstream_errors: self.upstream_errors.wrapping_sub(earlier.upstream_errors),
            latency: self.latency.since(&earlier.latency),
        }
    }

    fn is_empty(&self) -> bool {
        self.statuses.total() == 0 && self.upstream_errors == 0 && self.latency.is_empty()
    }
}

/// What one series counted between two drains ([`Traffic::drain`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficDelta {
    /// The route's host as the route table spells it; empty for unrouted requests.
    pub host: String,
    /// The route's prefix, e.g. `/` or `/api`; empty for unrouted requests.
    pub prefix: String,
    /// Who the route belonged to; `None` for unrouted requests.
    pub owner: Option<Owner>,
    /// Finished requests by status class.
    pub statuses: StatusCounts,
    /// Requests that failed because of their upstream.
    pub upstream_errors: u64,
    /// The latencies of the finished requests.
    pub latency: Histogram,
}

/// The counts of unrouted requests; routes fill in their host, prefix and owner.
impl From<Totals> for TrafficDelta {
    fn from(totals: Totals) -> Self {
        TrafficDelta {
            host: String::new(),
            prefix: String::new(),
            owner: None,
            statuses: totals.statuses,
            upstream_errors: totals.upstream_errors,
            latency: totals.latency,
        }
    }
}

/// The requests in flight at one moment: the one live traffic figure (ADR-0062 §8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InFlight {
    /// To each stack's routes, by stack name; stacks with none open are left out.
    stacks: BTreeMap<String, u64>,
    /// Requests that matched no route.
    unrouted: u64,
}

impl InFlight {
    /// The requests in flight to `stack`'s routes.
    pub fn stack(&self, stack: &str) -> u64 {
        self.stacks.get(stack).copied().unwrap_or_default()
    }

    /// The requests in flight to every stack's routes and unrouted, without hoster's own: what
    /// the dashboard's totals cover.
    pub fn hosted(&self) -> u64 {
        self.stacks.values().sum::<u64>() + self.unrouted
    }
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
    fn in_flight_requests_are_gauged_until_dropped() {
        let traffic = Traffic::default();
        let root = entry("/", "web");
        let mut first = traffic.route("app.example.com", &root);
        let second = traffic.route("app.example.com", &root);
        let gauge = |traffic: &Traffic| traffic.in_flight().stack("site");
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
        assert_eq!(gauge(&traffic), 0);
        assert_eq!(
            drained(&traffic),
            [(
                "app.example.com/ service `web` of stack `site`".to_owned(),
                1,
                0
            )]
        );
    }

    #[test]
    fn the_in_flight_gauge_counts_stacks_and_unrouted_but_not_hoster_itself() {
        let traffic = Traffic::default();
        let admin = RouteEntry {
            owner: Owner::Reserved("admin UI"),
            ..entry("/", "web")
        };
        let _site = traffic.route("app.example.com", &entry("/", "web"));
        let _api = traffic.route("app.example.com", &entry("/api", "api"));
        let _admin = traffic.route("admin.example.com", &admin);
        let _unrouted = traffic.unrouted();
        // The route changed hands with a request still open on the earlier owner's counters.
        let _moved = traffic.route("app.example.com", &entry("/api", "blog"));
        let in_flight = traffic.in_flight();
        assert_eq!(in_flight.stack("site"), 3);
        assert_eq!(in_flight.stack("other"), 0);
        assert_eq!(in_flight.hosted(), 4, "{in_flight:?}");
        assert_eq!(Traffic::default().in_flight(), InFlight::default());
    }

    /// The (host, prefix) of every route in the registry, sorted.
    fn registered(traffic: &Traffic) -> Vec<String> {
        let mut routes: Vec<String> = traffic
            .0
            .routes
            .load()
            .iter()
            .flat_map(|(host, prefixes)| {
                prefixes.keys().map(move |prefix| format!("{host}{prefix}"))
            })
            .collect();
        routes.sort();
        routes
    }

    fn drained(traffic: &Traffic) -> Vec<(String, u64, u64)> {
        traffic
            .drain()
            .into_iter()
            .map(|delta| {
                assert_eq!(delta.latency.samples(), delta.statuses.total(), "{delta:?}");
                let owner = delta.owner.map_or_else(String::new, |o| o.to_string());
                (
                    format!("{}{} {owner}", delta.host, delta.prefix),
                    delta.statuses.total(),
                    delta.upstream_errors,
                )
            })
            .collect()
    }

    #[test]
    fn a_drain_holds_what_came_since_the_previous_one() {
        let traffic = Traffic::default();
        let root = entry("/", "web");
        let api = entry("/api", "api");
        for _ in 0..3 {
            traffic
                .route("app.example.com", &root)
                .finish(outcome(Some(200), 5));
        }
        traffic
            .route("app.example.com", &api)
            .finish(RequestOutcome {
                upstream_error: true,
                ..outcome(Some(502), 1)
            });
        traffic.unrouted().finish(outcome(Some(404), 0));

        let first = traffic.drain();
        let [root_delta, api_delta, unrouted] = first.as_slice() else {
            panic!("{first:?}");
        };
        assert_eq!(
            (root_delta.host.as_str(), root_delta.prefix.as_str()),
            ("app.example.com", "/")
        );
        assert_eq!(root_delta.statuses.get(StatusClass::Success), 3);
        assert_eq!(root_delta.latency.samples(), 3);
        assert!(root_delta.latency.p50().unwrap() > Duration::from_millis(5));
        assert!(root_delta.owner.is_some());
        assert_eq!(api_delta.prefix, "/api");
        assert_eq!(api_delta.statuses.get(StatusClass::ServerError), 1);
        assert_eq!(api_delta.upstream_errors, 1);
        assert!(unrouted.owner.is_none());
        assert_eq!((unrouted.host.as_str(), unrouted.prefix.as_str()), ("", ""));
        assert_eq!(unrouted.statuses.get(StatusClass::ClientError), 1);

        traffic
            .route("app.example.com", &root)
            .finish(outcome(Some(404), 900));
        let second = traffic.drain();
        let [root_delta] = second.as_slice() else {
            panic!("{second:?}");
        };
        assert_eq!(root_delta.statuses.total(), 1);
        assert_eq!(root_delta.statuses.get(StatusClass::ClientError), 1);
        assert!(root_delta.latency.p95().unwrap() > Duration::from_millis(900));
        assert_eq!(traffic.drain(), [], "nothing since");
    }

    #[test]
    fn concurrent_requests_are_drained_exactly_once() {
        let traffic = Traffic::default();
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let traffic = traffic.clone();
                std::thread::spawn(move || {
                    let root = entry("/", "web");
                    for i in 0..2000 {
                        if i % 3 == 0 {
                            traffic.unrouted().finish(outcome(Some(400), 1));
                        } else {
                            traffic
                                .route("app.example.com", &root)
                                .finish(outcome(Some(200), i % 50));
                        }
                    }
                })
            })
            .collect();
        let mut total = Histogram::default();
        let mut count = |traffic: &Traffic| {
            for delta in traffic.drain() {
                total += delta.latency;
            }
        };
        while !threads.iter().all(|thread| thread.is_finished()) {
            count(&traffic);
        }
        for thread in threads {
            thread.join().unwrap();
        }
        count(&traffic);
        assert_eq!(total.samples(), 8000);
        assert_eq!(traffic.drain(), []);
    }

    #[test]
    fn a_route_that_changes_hands_is_a_new_series() {
        let traffic = Traffic::default();
        for _ in 0..2 {
            traffic
                .route("app.example.com", &entry("/", "web"))
                .finish(outcome(Some(200), 1));
        }
        traffic
            .route("app.example.com", &entry("/", "blog"))
            .finish(outcome(Some(200), 1));
        let mut deltas = drained(&traffic);
        deltas.sort();
        assert_eq!(
            deltas,
            [
                (
                    "app.example.com/ service `blog` of stack `site`".to_owned(),
                    1,
                    0
                ),
                (
                    "app.example.com/ service `web` of stack `site`".to_owned(),
                    2,
                    0
                ),
            ]
        );
        // Both idle now: out of the registry, then no longer followed.
        assert_eq!(traffic.drain(), []);
        assert!(registered(&traffic).is_empty());
        assert_eq!(traffic.drain(), []);
        assert!(traffic.0.drained.lock().unwrap().routes.is_empty());
    }

    #[test]
    fn idle_routes_leave_the_registry_once_drained() {
        let traffic = Traffic::default();
        let root = entry("/", "web");
        let busy = entry("/busy", "web");
        traffic
            .route("app.example.com", &root)
            .finish(outcome(Some(200), 1));
        let open = traffic.route("app.example.com", &busy);
        assert_eq!(drained(&traffic).len(), 1);
        assert_eq!(registered(&traffic).len(), 2);

        // Idle across this drain: out of the registry, followed until nothing holds it.
        assert_eq!(traffic.drain(), []);
        assert_eq!(
            registered(&traffic),
            ["app.example.com/busy"],
            "an in-flight request keeps its route"
        );
        assert_eq!(traffic.0.drained.lock().unwrap().routes.len(), 2);
        assert_eq!(traffic.drain(), []);
        assert_eq!(traffic.0.drained.lock().unwrap().routes.len(), 1);
        drop(open);

        // A request after that starts the route over, and counts.
        traffic
            .route("app.example.com", &root)
            .finish(outcome(Some(200), 1));
        assert_eq!(
            drained(&traffic),
            [(
                "app.example.com/ service `web` of stack `site`".to_owned(),
                1,
                0
            )]
        );
    }

    #[test]
    fn a_request_on_counters_leaving_the_registry_still_counts() {
        let traffic = Traffic::default();
        let root = entry("/", "web");
        // The proxy found the counters just before they left the registry.
        let counters = traffic.counters("app.example.com", &root);
        assert_eq!(traffic.drain(), []);
        assert!(registered(&traffic).is_empty());
        assert_eq!(traffic.drain(), []);
        counters.record(outcome(Some(200), 1));
        assert_eq!(drained(&traffic).len(), 1);
        drop(counters);
        assert_eq!(traffic.drain(), []);
        assert!(traffic.0.drained.lock().unwrap().routes.is_empty());
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
        let deltas = traffic.drain();
        let [root] = deltas.as_slice() else {
            panic!("{deltas:?}");
        };
        assert_eq!(root.statuses.get(StatusClass::Success), 2000);
        assert_eq!(traffic.in_flight().stack("site"), 0);
    }
}
