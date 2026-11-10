//! Route ownership (ADR-0012): a (host, normalized prefix) pair belongs to exactly one stack,
//! and reserved hosts (the admin host, ADR-0031) belong to hoster itself.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use super::upstream::UnixUpstream;
use crate::config::Config;
use crate::stack::{Counted, FieldPath, Host, Name, NormalizedPrefix, Stack};

/// The service that declares a route.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteOwner {
    pub stack: Name,
    pub service: Name,
}

impl fmt::Display for RouteOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "service `{}` of stack `{}`", self.service, self.stack)
    }
}

/// Whoever already holds a contested (host, prefix).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// A route of a stack: another stack, or an earlier route of the same stack.
    Route(RouteOwner),
    /// A host hoster keeps for itself; every prefix on it is taken. Carries the purpose.
    Reserved(&'static str),
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Owner::Route(owner) => owner.fmt(f),
            Owner::Reserved(purpose) => write!(f, "hoster ({purpose})"),
        }
    }
}

/// Hosts no stack may claim, at any prefix (ADR-0031).
///
/// A reserved host may also carry the Unix socket hoster serves it from (the admin app);
/// `RouteTable::build` then injects a `/` route to it. A host reserved without a socket is only
/// kept out of the stacks' reach and is not routed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reserved {
    hosts: BTreeMap<Host, ReservedHost>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReservedHost {
    purpose: &'static str,
    upstream: Option<Arc<UnixUpstream>>,
}

impl Reserved {
    /// The purpose of the admin host.
    const ADMIN_UI: &'static str = "admin UI";

    /// Reserves `host` for `purpose`, e.g. "admin UI", without routing it. Keeps a socket an
    /// earlier [`with_unix_host`](Self::with_unix_host) gave the same host.
    pub fn with_host(mut self, host: Host, purpose: &'static str) -> Self {
        let upstream = self.hosts.remove(&host).and_then(|h| h.upstream);
        self.hosts.insert(host, ReservedHost { purpose, upstream });
        self
    }

    /// Reserves `host` for `purpose` and routes every path on it to `upstream`.
    pub fn with_unix_host(
        mut self,
        host: Host,
        purpose: &'static str,
        upstream: Arc<UnixUpstream>,
    ) -> Self {
        let upstream = Some(upstream);
        self.hosts.insert(host, ReservedHost { purpose, upstream });
        self
    }

    fn owner(&self, host: &Host) -> Option<Owner> {
        self.hosts.get(host).map(|h| Owner::Reserved(h.purpose))
    }

    /// The reserved hosts served from a Unix socket, with their purpose.
    pub(super) fn served(&self) -> impl Iterator<Item = (&Host, &'static str, &Arc<UnixUpstream>)> {
        self.hosts
            .iter()
            .filter_map(|(host, h)| h.upstream.as_ref().map(|u| (host, h.purpose, u)))
    }
}

/// The reserved hosts of a running hoster: its admin host, routed to the admin app's Unix
/// socket in the state directory (ADR-0031). With `web.dev_listen` the app listens on loopback
/// TCP instead, so the host is only kept away from the stacks.
impl From<&Config> for Reserved {
    fn from(config: &Config) -> Self {
        let host = Host::from(&config.admin_host);
        match config.web.dev_listen {
            Some(_) => Reserved::default().with_host(host, Self::ADMIN_UI),
            None => Reserved::default().with_unix_host(
                host,
                Self::ADMIN_UI,
                Arc::new(UnixUpstream::from(
                    config.state_dir.join(Config::ADMIN_SOCKET_NAME),
                )),
            ),
        }
    }
}

/// One route that cannot be accepted because its (host, prefix) is already owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub host: Host,
    pub prefix: NormalizedPrefix,
    /// The route being rejected.
    pub claimant: RouteOwner,
    /// Who holds the pair.
    pub owner: Owner,
}

impl Collision {
    /// Where the rejected route is in `candidate`'s file, the stack that claimed it: its
    /// `host`, `services.<service>.routes[<i>].host`. Of two routes with the same host and
    /// prefix, the later one is rejected.
    pub fn path(&self, candidate: &Stack) -> FieldPath {
        let service = FieldPath::default()
            .key("services")
            .key(self.claimant.service.as_ref());
        let index = candidate
            .services
            .get(&self.claimant.service)
            .and_then(|s| {
                s.routes
                    .iter()
                    .rposition(|r| r.host == self.host && r.prefix == self.prefix)
            });
        match index {
            Some(index) => service.key("routes").index(index).key("host"),
            None => service.key("routes"),
        }
    }
}

impl fmt::Display for Collision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Collision {
            host,
            prefix,
            claimant,
            owner,
        } = self;
        match owner {
            Owner::Reserved(_) => write!(
                f,
                "route `{host}{prefix}` of {claimant} uses host `{host}`, which is reserved \
                 for {owner}"
            ),
            Owner::Route(existing) if existing.stack == claimant.stack => write!(
                f,
                "route `{host}{prefix}` of {claimant} duplicates the same route of {existing}"
            ),
            Owner::Route(existing) => write!(
                f,
                "route `{host}{prefix}` of {claimant} is already owned by {existing}"
            ),
        }
    }
}

/// Every collision found; never empty.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct CollisionError {
    pub collisions: Vec<Collision>,
}

impl fmt::Display for CollisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:", Counted(self.collisions.len(), "route collision"))?;
        self.collisions
            .iter()
            .try_for_each(|collision| write!(f, "\n  - {collision}"))
    }
}

/// Who owns which (host, prefix). Build it with [`RouteClaims::check`] for one candidate stack
/// (validation of a deploy or apply) or [`RouteClaims::check_all`] for a whole desired state
/// (`RouteTable::build`, the last line of defense).
#[derive(Debug, Clone, Default)]
pub struct RouteClaims {
    reserved: Reserved,
    owners: BTreeMap<(Host, NormalizedPrefix), RouteOwner>,
}

impl RouteClaims {
    /// Checks `candidate` against the routes of `others` and the `reserved` hosts, and against
    /// its own routes for duplicates. Only collisions involving the candidate are reported.
    ///
    /// A stack in `others` with the candidate's name is the version being replaced and is
    /// ignored, so re-applying a stack never collides with itself.
    pub fn check<'a>(
        candidate: &Stack,
        others: impl IntoIterator<Item = &'a Stack>,
        reserved: &Reserved,
    ) -> Result<(), CollisionError> {
        let mut claims = Self::new(reserved);
        for other in others.into_iter().filter(|o| o.name != candidate.name) {
            // Earlier problems among the others are not the candidate's to report.
            let _ = claims.claim_stack(other);
        }
        claims.claim_stack(candidate)
    }

    /// Claims the routes of every stack in turn and reports every collision. On success,
    /// returns the complete ownership map.
    pub fn check_all<'a>(
        stacks: impl IntoIterator<Item = &'a Stack>,
        reserved: &Reserved,
    ) -> Result<Self, CollisionError> {
        let mut claims = Self::new(reserved);
        let mut collisions = Vec::new();
        for stack in stacks {
            if let Err(err) = claims.claim_stack(stack) {
                collisions.extend(err.collisions);
            }
        }
        if collisions.is_empty() {
            Ok(claims)
        } else {
            Err(CollisionError { collisions })
        }
    }

    /// The owner of an exact (host, prefix), if any.
    pub fn owner(&self, host: &Host, prefix: &NormalizedPrefix) -> Option<Owner> {
        self.reserved.owner(host).or_else(|| {
            self.owners
                .get(&(host.clone(), prefix.clone()))
                .cloned()
                .map(Owner::Route)
        })
    }

    fn new(reserved: &Reserved) -> Self {
        Self {
            reserved: reserved.clone(),
            owners: BTreeMap::new(),
        }
    }

    /// Claims every route of `stack`; a route whose pair is taken is reported and not claimed.
    fn claim_stack(&mut self, stack: &Stack) -> Result<(), CollisionError> {
        let mut collisions = Vec::new();
        for (service_name, service) in &stack.services {
            let claimant = RouteOwner {
                stack: stack.name.clone(),
                service: service_name.clone(),
            };
            for route in &service.routes {
                if let Some(owner) = self.owner(&route.host, &route.prefix) {
                    collisions.push(Collision {
                        host: route.host.clone(),
                        prefix: route.prefix.clone(),
                        claimant: claimant.clone(),
                        owner,
                    });
                } else {
                    self.owners
                        .insert((route.host.clone(), route.prefix.clone()), claimant.clone());
                }
            }
        }
        if collisions.is_empty() {
            Ok(())
        } else {
            Err(CollisionError { collisions })
        }
    }
}
