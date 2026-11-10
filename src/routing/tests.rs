use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::stack::{
    DEFAULT_DRAIN_TIMEOUT, DEFAULT_STOP_TIMEOUT, Host, Name, NormalizedPrefix, Route,
    RouteTimeouts, Service, Stack, UpdateStrategy,
};

fn name(s: &str) -> Name {
    s.parse().expect("valid name")
}

fn host(s: &str) -> Host {
    s.parse().expect("valid host")
}

fn prefix(s: &str) -> NormalizedPrefix {
    s.parse().expect("valid prefix")
}

/// A stack whose services declare the given `(service, host, prefix)` routes.
fn stack(stack_name: &str, routes: &[(&str, &str, &str)]) -> Stack {
    let mut services: BTreeMap<Name, Service> = BTreeMap::new();
    for &(service, h, p) in routes {
        services
            .entry(name(service))
            .or_insert_with(|| Service {
                image: "ghcr.io/acme/app".parse().expect("valid image"),
                depends_on: Default::default(),
                track: None,
                command: None,
                entrypoint: None,
                env: BTreeMap::new(),
                secrets: BTreeMap::new(),
                volumes: BTreeMap::new(),
                files: BTreeMap::new(),
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
                prefix: prefix(p),
                strip_prefix: false,
                port: 8080,
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

fn admin() -> Reserved {
    Reserved::default().with_host(host("hoster.example.com"), "admin UI")
}

fn owner(stack: &str, service: &str) -> RouteOwner {
    RouteOwner {
        stack: name(stack),
        service: name(service),
    }
}

fn collisions(result: Result<(), CollisionError>) -> Vec<Collision> {
    result.map_or_else(|err| err.collisions, |()| Vec::new())
}

#[test]
fn disjoint_stacks_pass() {
    let a = stack("a", &[("web", "a.example.com", "/")]);
    let b = stack("b", &[("web", "b.example.com", "/")]);
    assert_eq!(RouteClaims::check(&b, [&a], &admin()), Ok(()));
}

#[test]
fn different_prefixes_on_one_host_may_belong_to_different_stacks() {
    let a = stack("a", &[("web", "example.com", "/")]);
    let b = stack(
        "b",
        &[
            ("blog", "example.com", "/blog"),
            ("api", "example.com", "/blog/api"),
        ],
    );
    assert_eq!(RouteClaims::check(&b, [&a], &admin()), Ok(()));
    assert_eq!(RouteClaims::check(&a, [&b], &admin()), Ok(()));
}

#[test]
fn same_host_and_prefix_across_stacks_collides_naming_both_owners() {
    let a = stack("a", &[("web", "example.com", "/api")]);
    let b = stack("b", &[("api", "example.com", "/api")]);
    let err = RouteClaims::check(&b, [&a], &admin()).unwrap_err();
    assert_eq!(
        err.collisions,
        vec![Collision {
            host: host("example.com"),
            prefix: prefix("/api"),
            claimant: owner("b", "api"),
            owner: Owner::Route(owner("a", "web")),
        }]
    );
    assert_eq!(
        err.to_string(),
        "1 route collision:\n  - route `example.com/api` of service `api` of stack `b` is \
         already owned by service `web` of stack `a`"
    );
}

#[test]
fn every_collision_is_reported() {
    let a = stack(
        "a",
        &[("web", "example.com", "/"), ("web", "example.com", "/x")],
    );
    let b = stack(
        "b",
        &[
            ("one", "example.com", "/"),
            ("two", "example.com", "/x"),
            ("two", "hoster.example.com", "/y"),
            ("two", "other.example.com", "/"),
        ],
    );
    let err = RouteClaims::check(&b, [&a], &admin()).unwrap_err();
    assert_eq!(err.collisions.len(), 3, "{err}");
}

#[test]
fn duplicate_within_one_stack_is_rejected() {
    let a = stack(
        "a",
        &[
            ("web", "example.com", "/api"),
            ("api", "example.com", "/api"),
        ],
    );
    let err = RouteClaims::check(&a, [], &admin()).unwrap_err();
    // Services are visited in name order: `api` claims first, `web` duplicates it.
    assert_eq!(
        err.collisions,
        vec![Collision {
            host: host("example.com"),
            prefix: prefix("/api"),
            claimant: owner("a", "web"),
            owner: Owner::Route(owner("a", "api")),
        }]
    );
    assert!(
        err.to_string().contains("duplicates the same route"),
        "{err}"
    );
}

#[test]
fn duplicate_within_one_service_is_rejected() {
    let a = stack(
        "a",
        &[("web", "example.com", "/"), ("web", "example.com", "/")],
    );
    assert_eq!(collisions(RouteClaims::check(&a, [], &admin())).len(), 1);
}

#[test]
fn reserved_host_is_never_claimable_at_any_prefix() {
    for p in ["/", "/hooks", "/some/deep/path"] {
        let a = stack("a", &[("web", "hoster.example.com", p)]);
        let err = RouteClaims::check(&a, [], &admin()).unwrap_err();
        assert_eq!(err.collisions[0].owner, Owner::Reserved("admin UI"));
        assert!(
            err.to_string()
                .contains("which is reserved for hoster (admin UI)"),
            "{err}"
        );
    }
    // Subdomains of the admin host are ordinary hosts.
    let a = stack("a", &[("web", "app.hoster.example.com", "/")]);
    assert_eq!(RouteClaims::check(&a, [], &admin()), Ok(()));
}

#[test]
#[allow(
    clippy::result_large_err,
    reason = "figment::Jail closures return figment::Error"
)]
fn reserved_comes_from_the_configured_admin_host() {
    figment::Jail::expect_with(|jail| {
        jail.create_file("hoster.toml", "admin_host = \"admin.example.org\"\n")?;
        jail.set_env("HOME", jail.directory().to_owned().display());
        jail.set_env("XDG_DATA_HOME", jail.directory().join("data").display());
        jail.set_env("HOSTER_ADMIN_GITHUB", "octocat");
        let config = crate::config::Config::load(Some(&jail.directory().join("hoster.toml")))
            .expect("valid config");
        let reserved = Reserved::from(&config);

        for p in ["/", "/admin"] {
            let a = stack("a", &[("web", "admin.example.org", p)]);
            let err = RouteClaims::check(&a, [], &reserved).unwrap_err();
            assert_eq!(err.collisions[0].owner, Owner::Reserved("admin UI"));
        }
        // Only the configured host is reserved, not some default.
        let a = stack("a", &[("web", "hoster.example.com", "/")]);
        assert_eq!(RouteClaims::check(&a, [], &reserved), Ok(()));

        // Every path on it goes to the admin app's socket in the state directory.
        let table = RouteTable::build(&DesiredState::default(), &reserved).unwrap();
        let entry = table.lookup("admin.example.org", "/hooks/deploy").unwrap();
        let socket = config.state_dir.join("admin.sock");
        assert!(
            matches!(&entry.target, Target::Unix(u) if u.as_ref().as_ref() == socket),
            "{entry:?}"
        );

        // In development the app listens on loopback TCP: the host stays reserved, unrouted.
        jail.set_env("HOSTER_WEB__DEV_LISTEN", "127.0.0.1:8080");
        let config = crate::config::Config::load(Some(&jail.directory().join("hoster.toml")))
            .expect("valid config");
        let reserved = Reserved::from(&config);
        let a = stack("a", &[("web", "admin.example.org", "/")]);
        assert!(RouteClaims::check(&a, [], &reserved).is_err());
        let table = RouteTable::build(&DesiredState::default(), &reserved).unwrap();
        assert!(table.lookup("admin.example.org", "/").is_none());
        Ok(())
    });
}

#[test]
fn reapplying_a_stack_does_not_collide_with_its_previous_version() {
    let old = stack("a", &[("web", "example.com", "/")]);
    let new = stack(
        "a",
        &[("web", "example.com", "/"), ("web", "example.com", "/v2")],
    );
    assert_eq!(RouteClaims::check(&new, [&old], &admin()), Ok(()));
}

#[test]
fn collisions_among_others_are_not_the_candidates_problem() {
    let a = stack("a", &[("web", "example.com", "/")]);
    let b = stack("b", &[("web", "example.com", "/")]);
    let c = stack("c", &[("web", "example.com", "/c")]);
    assert_eq!(RouteClaims::check(&c, [&a, &b], &admin()), Ok(()));
}

#[test]
fn check_all_reports_every_collision_and_returns_owners() {
    let a = stack("a", &[("web", "example.com", "/")]);
    let b = stack("b", &[("web", "example.com", "/blog")]);
    let claims = RouteClaims::check_all([&a, &b], &admin()).expect("no collisions");
    assert_eq!(
        claims.owner(&host("example.com"), &prefix("/blog")),
        Some(Owner::Route(owner("b", "web")))
    );
    assert_eq!(
        claims.owner(&host("hoster.example.com"), &NormalizedPrefix::root()),
        Some(Owner::Reserved("admin UI"))
    );
    assert_eq!(claims.owner(&host("example.com"), &prefix("/x")), None);

    let c = stack(
        "c",
        &[
            ("web", "example.com", "/"),
            ("web", "hoster.example.com", "/"),
        ],
    );
    let err = RouteClaims::check_all([&a, &b, &c], &admin()).unwrap_err();
    assert_eq!(err.collisions.len(), 2, "{err}");
}

const HOSTS: [&str; 3] = ["example.com", "www.example.com", "hoster.example.com"];
const SEGMENTS: [&str; 3] = ["api", "v1", "blog"];
const SERVICES: [&str; 2] = ["web", "api"];

/// `(service, host, prefix)` route declarations drawn from a small pool so collisions are common.
fn routes() -> impl Strategy<Value = Vec<(&'static str, &'static str, String)>> {
    let route = (
        prop::sample::select(&SERVICES[..]),
        prop::sample::select(&HOSTS[..]),
        prop::collection::vec(prop::sample::select(&SEGMENTS[..]), 0..3),
    )
        .prop_map(|(service, host, segments)| {
            let prefix = segments.iter().map(|s| format!("/{s}")).collect::<String>();
            (
                service,
                host,
                if prefix.is_empty() {
                    "/".into()
                } else {
                    prefix
                },
            )
        });
    prop::collection::vec(route, 0..5)
}

fn build(stack_name: &str, routes: &[(&str, &str, String)]) -> Stack {
    let borrowed: Vec<(&str, &str, &str)> = routes
        .iter()
        .map(|(s, h, p)| (*s, *h, p.as_str()))
        .collect();
    stack(stack_name, &borrowed)
}

/// The set of contested (host, prefix) pairs between `a` and `b` alone, ignoring duplicates
/// inside either one.
fn cross_pairs(a: &Stack, b: &Stack) -> Vec<(Host, NormalizedPrefix)> {
    let mut pairs: Vec<_> = collisions(RouteClaims::check(b, [a], &Reserved::default()))
        .into_iter()
        .filter(|c| c.owner_stack() == Some(&a.name))
        .map(|c| (c.host, c.prefix))
        .collect();
    pairs.sort();
    pairs.dedup();
    pairs
}

impl Collision {
    fn owner_stack(&self) -> Option<&Name> {
        match &self.owner {
            Owner::Route(owner) => Some(&owner.stack),
            Owner::Reserved(_) => None,
        }
    }
}

proptest! {
    /// Collision keys are normalized by construction: an accepted prefix has exactly one
    /// spelling, so two spellings of the same segments (`/api` and `/api/`, `/a//b`, `/./a`)
    /// can never slip past the check as different keys.
    #[test]
    fn accepted_prefixes_have_one_spelling(s in "[/a-zA-Z.%?#~-]{0,10}") {
        if let Ok(p) = s.parse::<NormalizedPrefix>() {
            prop_assert_eq!(p.to_string(), s.clone());
            let segments: Vec<&str> = s.split('/').skip(1).filter(|seg| !seg.is_empty()).collect();
            prop_assert_eq!(p.segments(), segments.as_slice());
        }
    }

    /// An accepted host is already in the request-side normal form of §6.2 (lowercase, no port,
    /// no trailing dot), so a route host and a request host compare exactly.
    #[test]
    fn accepted_hosts_are_request_normalized(s in "[a-zA-Z0-9.:*/-]{1,20}") {
        if let Ok(h) = s.parse::<Host>() {
            let request_form = s.to_ascii_lowercase();
            let request_form = request_form.split(':').next().unwrap_or_default();
            prop_assert_eq!(h.as_ref(), request_form.trim_end_matches('.'));
            prop_assert_eq!(h.as_ref(), s.as_str());
        }
    }

    /// Whether A and B contest a (host, prefix) does not depend on which one is the candidate.
    #[test]
    fn cross_stack_collisions_are_symmetric(a in routes(), b in routes()) {
        let a = build("a", &a);
        let b = build("b", &b);
        prop_assert_eq!(cross_pairs(&a, &b), cross_pairs(&b, &a));
    }

    /// A clean check either way round means a clean check of both together, and vice versa.
    #[test]
    fn check_agrees_with_check_all(a in routes(), b in routes()) {
        let a = build("a", &a);
        let b = build("b", &b);
        let reserved = admin();
        let pairwise = RouteClaims::check(&a, [], &reserved).is_ok()
            && RouteClaims::check(&b, [&a], &reserved).is_ok();
        let flipped = RouteClaims::check(&b, [], &reserved).is_ok()
            && RouteClaims::check(&a, [&b], &reserved).is_ok();
        let all = RouteClaims::check_all([&a, &b], &reserved).is_ok();
        prop_assert_eq!(pairwise, all);
        prop_assert_eq!(flipped, all);
    }

    /// The check sees only normalized keys: re-spelling each route through its canonical
    /// `Display` form and reordering the other stacks changes nothing.
    #[test]
    fn check_is_normalization_invariant(a in routes(), b in routes(), c in routes()) {
        let a = build("a", &a);
        let b = build("b", &b);
        let c = build("c", &c);
        let respelled: Vec<_> = c
            .services
            .iter()
            .flat_map(|(service, s)| {
                s.routes.iter().map(move |r| {
                    let h: Host = r.host.to_string().parse().expect("round-trips");
                    let p: NormalizedPrefix = r.prefix.to_string().parse().expect("round-trips");
                    prop_assert_eq!(&h, &r.host);
                    prop_assert_eq!(p.segments(), r.prefix.segments());
                    Ok((service.to_string(), h.to_string(), p.to_string()))
                })
            })
            .collect::<Result<_, _>>()?;
        let borrowed: Vec<(&str, &str, &str)> = respelled
            .iter()
            .map(|(s, h, p)| (s.as_str(), h.as_str(), p.as_str()))
            .collect();
        let c2 = stack("c", &borrowed);
        let reserved = admin();
        let forward = RouteClaims::check(&c, [&a, &b], &reserved).is_ok();
        prop_assert_eq!(forward, RouteClaims::check(&c2, [&b, &a], &reserved).is_ok());
    }
}
