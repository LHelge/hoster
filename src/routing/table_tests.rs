//! `RouteTable` build, lookup, path rewriting and diff.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::*;
use crate::stack::{
    DEFAULT_DRAIN_TIMEOUT, DEFAULT_STOP_TIMEOUT, Host, Name, Route, RouteTimeouts, Service, Stack,
    UpdateStrategy,
};

fn name(s: &str) -> Name {
    s.parse().expect("valid name")
}

fn host(s: &str) -> Host {
    s.parse().expect("valid host")
}

/// One declared route: (service, host, prefix, container port, strip_prefix).
type Decl<'a> = (&'a str, &'a str, &'a str, u16, bool);

fn stack(stack_name: &str, routes: &[Decl<'_>]) -> Stack {
    let mut services: BTreeMap<Name, Service> = BTreeMap::new();
    for &(service, h, p, port, strip_prefix) in routes {
        services
            .entry(name(service))
            .or_insert_with(|| Service {
                image: "ghcr.io/acme/app".parse().expect("valid image"),
                track: None,
                command: None,
                entrypoint: None,
                env: BTreeMap::new(),
                secrets: BTreeMap::new(),
                volumes: BTreeMap::new(),
                health: None,
                update: UpdateStrategy::default(),
                stop_timeout: DEFAULT_STOP_TIMEOUT,
                user: None,
                read_only: false,
                routes: Vec::new(),
            })
            .routes
            .push(Route {
                host: host(h),
                prefix: p.parse().expect("valid prefix"),
                strip_prefix,
                port,
                timeouts: RouteTimeouts::default(),
            });
    }
    Stack {
        name: name(stack_name),
        drain_timeout: DEFAULT_DRAIN_TIMEOUT,
        deploy: None,
        services,
    }
}

/// A desired stack where every service is live, with one upstream per declared container port
/// on host port `base + n`.
fn live(stack_name: &str, routes: &[Decl<'_>], base: u16) -> DesiredStack {
    let stack = stack(stack_name, routes);
    let mut desired = DesiredStack::new(stack.clone());
    let mut next = base;
    for (service_name, service) in &stack.services {
        let ports: BTreeSet<u16> = service.routes.iter().map(|r| r.port).collect();
        let upstreams = ports.into_iter().map(|port| {
            next += 1;
            let upstream = Upstream::loopback(next, stack.name.clone(), service_name.clone(), 1);
            (port, Arc::new(upstream))
        });
        let upstreams: Vec<_> = upstreams.collect();
        desired = desired.with_live(service_name.clone(), upstreams);
    }
    desired
}

fn admin_socket() -> Arc<UnixUpstream> {
    Arc::new(UnixUpstream::from(PathBuf::from(
        "/var/lib/hoster/admin.sock",
    )))
}

fn admin() -> Reserved {
    Reserved::default().with_unix_host(host("hoster.example.com"), "admin UI", admin_socket())
}

/// Frontend at `/`, API at `/api` (stripped), S3 on its own host, plus a second stack owning
/// `/blog` on the shared host.
fn example() -> RouteTable {
    let desired: DesiredState = [
        live(
            "shop",
            &[
                ("web", "example.com", "/", 3000, false),
                ("api", "example.com", "/api", 8080, true),
                ("api", "example.com", "/api/v2/admin", 8080, false),
                ("s3", "s3.example.com", "/", 9000, false),
            ],
            20000,
        ),
        live("blog", &[("web", "example.com", "/blog", 80, false)], 21000),
    ]
    .into_iter()
    .collect();
    RouteTable::build(&desired, &admin()).expect("no collisions")
}

/// `(stack, service)` of the matched route, or `"hoster"` for a reserved one.
fn owner_of(entry: Option<&RouteEntry>) -> Option<String> {
    entry.map(|entry| match &entry.owner {
        Owner::Route(owner) => format!("{}/{}", owner.stack, owner.service),
        Owner::Reserved(_) => "hoster".to_owned(),
    })
}

#[test]
fn lookup_picks_the_longest_segment_prefix() {
    let table = example();
    let cases: &[(&str, &str, Option<&str>)] = &[
        ("example.com", "/", Some("shop/web")),
        ("example.com", "/index.html", Some("shop/web")),
        ("example.com", "/api", Some("shop/api")),
        ("example.com", "/api/", Some("shop/api")),
        ("example.com", "/api/users", Some("shop/api")),
        ("example.com", "/api?x=1", Some("shop/api")),
        ("example.com", "/api/?x=1", Some("shop/api")),
        ("example.com", "/apiary", Some("shop/web")),
        ("example.com", "/api%2Fusers", Some("shop/web")),
        ("example.com", "/api/v2/admin/x", Some("shop/api")),
        ("example.com", "/api/v2/administrator", Some("shop/api")),
        ("example.com", "/blog", Some("blog/web")),
        ("example.com", "/blog/post?id=1", Some("blog/web")),
        ("example.com", "/blogs", Some("shop/web")),
        ("example.com", "", Some("shop/web")),
        ("s3.example.com", "/bucket/key", Some("shop/s3")),
        ("hoster.example.com", "/", Some("hoster")),
        ("hoster.example.com", "/hooks/deploy", Some("hoster")),
        ("unknown.example.com", "/", None),
        ("example.org", "/api", None),
    ];
    for &(h, path, expected) in cases {
        assert_eq!(
            owner_of(table.lookup(h, path)).as_deref(),
            expected,
            "{h}{path}"
        );
    }
    let deep = table.lookup("example.com", "/api/v2/admin/x").unwrap();
    assert_eq!(deep.prefix_str, "/api/v2/admin");
}

#[test]
fn lookup_without_a_root_route_has_gaps() {
    let desired: DesiredState = [live("a", &[("api", "a.example.com", "/api", 80, false)], 0)]
        .into_iter()
        .collect();
    let table = RouteTable::build(&desired, &Reserved::default()).unwrap();
    assert!(table.lookup("a.example.com", "/").is_none());
    assert!(table.lookup("a.example.com", "/apiary").is_none());
    assert!(table.lookup("a.example.com", "/api/x").is_some());
}

#[test]
fn lookup_normalizes_the_request_host() {
    let table = example();
    for h in [
        "example.com",
        "EXAMPLE.com",
        "example.com:443",
        "example.com.",
        "Example.COM.:8443",
        " example.com ",
    ] {
        assert_eq!(
            owner_of(table.lookup(h, "/api")).as_deref(),
            Some("shop/api"),
            "{h:?}"
        );
    }
    for h in ["", "example", "[::1]:443", "127.0.0.1", "www.example.com"] {
        assert!(table.lookup(h, "/").is_none(), "{h:?}");
    }
}

#[test]
fn known_host_answers_in_table_spelling() {
    let table = example();
    for h in ["example.com", "EXAMPLE.com.:80", "example.com:443"] {
        assert_eq!(table.known_host(h), Some("example.com"), "{h:?}");
    }
    assert_eq!(
        table.known_host("Hoster.Example.com"),
        Some("hoster.example.com"),
        "reserved hosts are known"
    );
    for h in ["", "www.example.com", "[::1]:80"] {
        assert!(table.known_host(h).is_none(), "{h:?}");
    }
    assert_eq!(
        RouteTable::normalize_host("A.Example.COM.:8443"),
        "a.example.com"
    );
}

#[test]
fn targets_are_loopback_unix_or_unavailable() {
    let table = example();
    let Target::Loopback(upstream) = &table.lookup("example.com", "/api").unwrap().target else {
        panic!("expected a loopback target");
    };
    assert!(upstream.addr.ip().is_loopback());
    assert_eq!(upstream.service, name("api"));

    let Target::Unix(socket) = &table.lookup("hoster.example.com", "/").unwrap().target else {
        panic!("expected the admin socket");
    };
    assert_eq!(socket.as_ref(), admin_socket().as_ref());

    let retry_after = Duration::from_secs(5);
    let desired: DesiredState =
        [
            DesiredStack::new(stack("db", &[("pg", "db.example.com", "/", 5432, false)]))
                .with_gap(name("pg"), retry_after),
        ]
        .into_iter()
        .collect();
    let table = RouteTable::build(&desired, &admin()).unwrap();
    assert!(matches!(
        table.lookup("db.example.com", "/").unwrap().target,
        Target::Unavailable { retry_after: r } if r == retry_after
    ));
}

#[test]
fn the_admin_route_is_injected_by_build_only_for_a_served_reserved_host() {
    let table = RouteTable::build(&DesiredState::default(), &admin()).unwrap();
    let entry = table.lookup("hoster.example.com", "/anything").unwrap();
    assert_eq!(entry.owner, Owner::Reserved("admin UI"));
    assert_eq!(entry.prefix_str, "/");

    let unrouted = Reserved::default().with_host(host("hoster.example.com"), "admin UI");
    let table = RouteTable::build(&DesiredState::default(), &unrouted).unwrap();
    assert!(table.lookup("hoster.example.com", "/").is_none());
}

#[test]
fn services_without_a_running_generation_get_no_routes() {
    let declared = stack(
        "a",
        &[
            ("web", "a.example.com", "/", 80, false),
            ("api", "a.example.com", "/api", 8080, false),
        ],
    );
    let upstream = Arc::new(Upstream::loopback(20001, name("a"), name("web"), 3));
    // `api` was never deployed; `web` has no upstream for a port `api` would use.
    let desired: DesiredState =
        [DesiredStack::new(declared).with_live(name("web"), [(80, upstream)])]
            .into_iter()
            .collect();
    let table = RouteTable::build(&desired, &Reserved::default()).unwrap();
    assert_eq!(
        owner_of(table.lookup("a.example.com", "/api")).as_deref(),
        Some("a/web")
    );
}

#[test]
fn build_rejects_collisions() {
    let desired: DesiredState = [
        live("a", &[("web", "example.com", "/api", 80, false)], 0),
        live("b", &[("web", "example.com", "/api", 80, false)], 100),
        live("c", &[("web", "hoster.example.com", "/x", 80, false)], 200),
    ]
    .into_iter()
    .collect();
    let err = RouteTable::build(&desired, &admin()).unwrap_err();
    assert_eq!(err.collisions.len(), 2);
}

#[test]
fn upstream_path_strips_on_request_and_keeps_the_query() {
    let table = example();
    let cases: &[(&str, &str)] = &[
        ("/api/users?x=1", "/users?x=1"),
        ("/api", "/"),
        ("/api/", "/"),
        ("/api?x=1", "/?x=1"),
        ("/api/?x=1", "/?x=1"),
        ("/api/a/b", "/a/b"),
    ];
    for &(request, expected) in cases {
        let entry = table.lookup("example.com", request).unwrap();
        assert!(entry.strip_prefix);
        assert_eq!(entry.upstream_path(request), expected, "{request}");
        assert_eq!(entry.forwarded_prefix(), Some("/api"));
    }
    // Not stripped: unchanged, including the root route.
    let blog = table.lookup("example.com", "/blog/x?y").unwrap();
    assert_eq!(blog.upstream_path("/blog/x?y"), "/blog/x?y");
    assert_eq!(blog.forwarded_prefix(), Some("/blog"));
    let root = table.lookup("example.com", "/x").unwrap();
    assert_eq!(root.upstream_path("/x"), "/x");
    assert_eq!(root.forwarded_prefix(), None);
}

#[test]
fn versions_increase_per_build() {
    assert_eq!(RouteTable::default().version(), 0);
    let a = example();
    let b = example();
    assert!(a.version() > 0);
    assert!(b.version() > a.version());
}

#[test]
fn diff_names_added_removed_and_retargeted_routes() {
    let before = example();
    // Redeploy `api` (new upstreams), drop the blog stack, add a route.
    let desired: DesiredState = [live(
        "shop",
        &[
            ("web", "example.com", "/", 3000, false),
            ("api", "example.com", "/api", 8080, true),
            ("api", "example.com", "/api/v2/admin", 8080, false),
            ("s3", "s3.example.com", "/", 9000, false),
            ("s3", "s3.example.com", "/console", 9001, false),
        ],
        22000,
    )]
    .into_iter()
    .collect();
    let after = RouteTable::build(&desired, &admin()).unwrap();
    let diff = after.diff(&before);
    assert_eq!(diff.added, ["s3.example.com/console"]);
    assert_eq!(diff.removed, ["example.com/blog"]);
    // Every live route got a fresh upstream; the admin route is unchanged.
    assert_eq!(
        diff.changed,
        [
            "example.com/",
            "example.com/api",
            "example.com/api/v2/admin",
            "s3.example.com/"
        ]
    );
    assert_eq!(
        diff.to_string(),
        format!(
            "route table v{} -> v{}: 1 added, 1 removed, 4 changed +s3.example.com/console \
             -example.com/blog ~example.com/ ~example.com/api ~example.com/api/v2/admin \
             ~s3.example.com/",
            before.version(),
            after.version()
        )
    );
    assert!(after.diff(&after).is_empty());
}

#[test]
fn diff_keeps_shared_upstreams_unchanged() {
    let desired: DesiredState = [live("a", &[("web", "a.example.com", "/", 80, false)], 0)]
        .into_iter()
        .collect();
    let first = RouteTable::build(&desired, &admin()).unwrap();
    let second = RouteTable::build(&desired, &admin()).unwrap();
    assert!(second.diff(&first).is_empty());
}

#[test]
fn inflight_guards_count_requests_across_tables() {
    let table = example();
    let Target::Loopback(upstream) = &table.lookup("example.com", "/").unwrap().target else {
        panic!("expected a loopback target");
    };
    let held = Arc::clone(upstream);
    let first = upstream.track();
    let second = held.track();
    assert_eq!(held.inflight(), 2);
    drop(table);
    drop(first);
    assert_eq!(held.inflight(), 1);
    assert!(Arc::ptr_eq(second.upstream(), &held));
    drop(second);
    assert_eq!(held.inflight(), 0);
}

fn lone_upstream() -> Arc<Upstream> {
    Arc::new(Upstream::loopback(20000, name("site"), name("web"), 1))
}

#[tokio::test]
async fn drain_of_an_idle_upstream_returns_after_the_settle_delay() {
    let upstream = lone_upstream();
    let started = Instant::now();
    let outcome = upstream
        .drain(Duration::from_millis(50), Duration::from_secs(5))
        .await;
    assert_eq!(outcome, DrainOutcome::Drained);
    assert!(started.elapsed() >= Duration::from_millis(50));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn drain_waits_for_the_last_guard() {
    let upstream = lone_upstream();
    let first = upstream.track();
    let second = upstream.track();
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(first);
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(second);
    });
    let started = Instant::now();
    let outcome = upstream
        .drain(Duration::from_millis(10), Duration::from_secs(5))
        .await;
    assert_eq!(outcome, DrainOutcome::Drained);
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(upstream.inflight(), 0);
    releaser.await.unwrap();
}

#[tokio::test]
async fn drain_counts_requests_that_start_during_the_settle_delay() {
    let upstream = lone_upstream();
    let late = Arc::clone(&upstream);
    let request = tokio::spawn(async move {
        // Loaded the old table before the swap, tracked just after it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let guard = late.track();
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(guard);
    });
    let started = Instant::now();
    let outcome = upstream
        .drain(Duration::from_millis(100), Duration::from_secs(5))
        .await;
    assert_eq!(outcome, DrainOutcome::Drained);
    assert!(started.elapsed() >= Duration::from_millis(220));
    request.await.unwrap();
}

#[tokio::test]
async fn drain_times_out_with_the_requests_left() {
    let upstream = lone_upstream();
    let _stuck = [upstream.track(), upstream.track()];
    let started = Instant::now();
    let outcome = upstream
        .drain(Duration::from_millis(10), Duration::from_millis(100))
        .await;
    assert_eq!(outcome, DrainOutcome::TimedOut(2));
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn drain_timeout_includes_the_settle_delay() {
    let upstream = lone_upstream();
    let _stuck = upstream.track();
    let started = Instant::now();
    let outcome = upstream
        .drain(Duration::from_secs(30), Duration::from_millis(100))
        .await;
    assert_eq!(outcome, DrainOutcome::TimedOut(1));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn drain_accepts_a_timeout_too_large_for_an_instant() {
    let upstream = lone_upstream();
    let guard = upstream.track();
    let releaser = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(guard);
    });
    let outcome = upstream.drain(Duration::ZERO, Duration::MAX).await;
    assert_eq!(outcome, DrainOutcome::Drained);
    releaser.await.unwrap();
}

/// Sanity timing for the hot path; not a gate. `cargo test -- --ignored lookup_speed`.
#[test]
#[ignore = "timing sanity check, run manually"]
fn lookup_speed() {
    let table = example();
    let paths = [
        "/",
        "/api/users?x=1",
        "/api/v2/admin/x",
        "/apiary",
        "/blog/post",
    ];
    let iterations = 1_000_000;
    let start = Instant::now();
    for i in 0..iterations {
        std::hint::black_box(table.lookup("Example.com:443", paths[i % paths.len()]));
    }
    let per_lookup = start.elapsed() / iterations as u32;
    eprintln!("lookup: {per_lookup:?} per call");
    assert!(per_lookup < Duration::from_micros(10));
}

/// Segment-wise oracle for ADR-0009, independent of the string matching in `lookup`.
fn longest_matching(prefixes: &[Vec<&str>], path: &str) -> Option<usize> {
    let path = path.split('?').next().unwrap();
    let segments: Vec<&str> = path.strip_prefix('/')?.split('/').collect();
    let segments: &[&str] = if segments == [""] { &[] } else { &segments };
    prefixes
        .iter()
        .filter(|prefix| segments.starts_with(prefix))
        .map(Vec::len)
        .max()
}

proptest! {
    #[test]
    fn lookup_returns_the_longest_matching_prefix(
        prefixes in prop::collection::btree_set(
            prop::collection::vec(prop::sample::select(vec!["a", "b", "ab"]), 0..4),
            1..8,
        ),
        path in prop::collection::vec(
            prop::sample::select(vec!["a", "b", "ab", "ba", "", "a%2Fb"]), 0..5,
        ),
        query in prop::option::of("[a-z=/&]{0,6}"),
    ) {
        let prefixes: Vec<Vec<&str>> = prefixes.into_iter().collect();
        let routes: Vec<(String, String)> = prefixes
            .iter()
            .enumerate()
            .map(|(i, segments)| (format!("s{i}"), format!("/{}", segments.join("/"))))
            .collect();
        let decls: Vec<Decl<'_>> = routes
            .iter()
            .map(|(service, prefix)| (service.as_str(), "p.example.com", prefix.as_str(), 80, false))
            .collect();
        let desired: DesiredState = [live("p", &decls, 0)].into_iter().collect();
        let table = RouteTable::build(&desired, &Reserved::default()).unwrap();

        let mut request = format!("/{}", path.join("/"));
        if let Some(query) = &query {
            request.push('?');
            request.push_str(query);
        }
        let found = table.lookup("p.example.com", &request).map(|e| e.prefix.segments().len());
        prop_assert_eq!(found, longest_matching(&prefixes, &request));
        if let Some(entry) = table.lookup("p.example.com", &request) {
            let segments: Vec<&str> = entry.prefix.segments().iter().map(String::as_str).collect();
            prop_assert!(prefixes.contains(&segments));
        }
    }
}
