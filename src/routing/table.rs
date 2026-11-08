//! The immutable route table the proxy reads through `ArcSwap` (architecture §6.1, §6.2).

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::collision::{CollisionError, Owner, Reserved, RouteClaims, RouteOwner};
use super::desired::{DesiredState, ServiceState};
use super::upstream::{UnixUpstream, Upstream};
use crate::stack::{NormalizedPrefix, RouteTimeouts};

/// Every route hoster serves, grouped by host. Built whole from desired state by
/// [`RouteTable::build`] and never changed afterwards; the orchestrator publishes a new one.
#[derive(Debug, Default)]
pub struct RouteTable {
    /// Normalized host → its routes, longest prefix first.
    hosts: HashMap<String, Vec<RouteEntry>>,
    version: u64,
}

/// One (host, prefix) route.
#[derive(Debug, Clone)]
pub struct RouteEntry {
    /// Who declared the route: a stack's service, or hoster for the admin host.
    pub owner: Owner,
    pub prefix: NormalizedPrefix,
    /// The prefix as a path, e.g. `/api/v1` or `/`: for `X-Forwarded-Prefix` and stripping.
    pub prefix_str: String,
    pub strip_prefix: bool,
    pub target: Target,
    pub timeouts: RouteTimeouts,
}

/// Where a route's requests go.
#[derive(Debug, Clone)]
pub enum Target {
    /// A routed service on `127.0.0.1:<port>` (ADR-0007).
    Loopback(Arc<Upstream>),
    /// hoster's admin app on its Unix socket (ADR-0031).
    Unix(Arc<UnixUpstream>),
    /// A stop-first service in its gap: answer 503 with `Retry-After` (ADR-0039, ADR-0032).
    Unavailable { retry_after: Duration },
}

impl RouteTable {
    /// Builds the table for `desired`, plus a `/` route for every reserved host served from a
    /// Unix socket. Runs the collision check again first (ADR-0012), as the last line of
    /// defense. Every successful build gets a higher [`version`](Self::version) than the last.
    pub fn build(desired: &DesiredState, reserved: &Reserved) -> Result<Self, CollisionError> {
        RouteClaims::check_all(desired.stacks().map(|d| d.stack().as_ref()), reserved)?;

        let mut hosts: HashMap<String, Vec<RouteEntry>> = HashMap::new();
        for (host, purpose, upstream) in reserved.served() {
            hosts.entry(host.to_string()).or_default().push(RouteEntry {
                owner: Owner::Reserved(purpose),
                prefix: NormalizedPrefix::root(),
                prefix_str: "/".to_owned(),
                strip_prefix: false,
                target: Target::Unix(Arc::clone(upstream)),
                timeouts: RouteTimeouts::default(),
            });
        }
        for desired_stack in desired.stacks() {
            let stack = desired_stack.stack();
            for (service_name, service) in &stack.services {
                let Some(state) = desired_stack.services.get(service_name) else {
                    continue;
                };
                for route in &service.routes {
                    let target = match state {
                        ServiceState::Live { upstreams } => match upstreams.get(&route.port) {
                            Some(upstream) => Target::Loopback(Arc::clone(upstream)),
                            None => continue,
                        },
                        ServiceState::Gap { retry_after } => Target::Unavailable {
                            retry_after: *retry_after,
                        },
                    };
                    hosts
                        .entry(route.host.to_string())
                        .or_default()
                        .push(RouteEntry {
                            owner: Owner::Route(RouteOwner {
                                stack: stack.name.clone(),
                                service: service_name.clone(),
                            }),
                            prefix_str: route.prefix.to_string(),
                            prefix: route.prefix.clone(),
                            strip_prefix: route.strip_prefix,
                            target,
                            timeouts: route.timeouts,
                        });
                }
            }
        }
        for routes in hosts.values_mut() {
            routes.sort_by(|a, b| {
                (b.prefix.segments().len().cmp(&a.prefix.segments().len()))
                    .then_with(|| a.prefix.cmp(&b.prefix))
            });
        }

        static LAST_VERSION: AtomicU64 = AtomicU64::new(0);
        let version = LAST_VERSION.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(Self { hosts, version })
    }

    /// Increases with every build; 0 for the empty default table.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The route for a request: `host` as sent in `:authority` or `Host`, `path` as in the
    /// request target (a query is ignored).
    ///
    /// The host is lowercased and loses any port and trailing dot, then looked up exactly. Of
    /// its routes, the one with the longest prefix that matches whole path segments wins
    /// (ADR-0009): `/api` matches `/api`, `/api/` and `/api/x`, not `/apiary` or `/api%2Fx`.
    pub fn lookup(&self, host: &str, path: &str) -> Option<&RouteEntry> {
        let routes = self.hosts.get(Self::normalize_host(host).as_ref())?;
        let path = path.split(['?', '#']).next().unwrap_or_default();
        routes.iter().find(|entry| entry.matches(path))
    }

    /// The table's spelling of `host` (as sent in `:authority` or `Host`) if any route serves
    /// it: lowercase, without port or trailing dot. `None` for a host hoster does not know.
    pub fn known_host(&self, host: &str) -> Option<&str> {
        self.hosts
            .get_key_value(Self::normalize_host(host).as_ref())
            .map(|(host, _)| host.as_str())
    }

    /// What changed from `previous` to this table, for the log line of a publish.
    pub fn diff(&self, previous: &RouteTable) -> RouteDiff {
        let old = previous.keyed();
        let new = self.keyed();
        let mut diff = RouteDiff {
            from: previous.version,
            to: self.version,
            added: Vec::new(),
            removed: Vec::new(),
            changed: Vec::new(),
        };
        for (key, entry) in &new {
            match old.get(key) {
                None => diff.added.push(key.to_string()),
                Some(before) if !entry.same_as(before) => diff.changed.push(key.to_string()),
                Some(_) => {}
            }
        }
        diff.removed = old
            .keys()
            .filter(|key| !new.contains_key(*key))
            .map(ToString::to_string)
            .collect();
        diff
    }

    /// Every entry keyed by (host, prefix), in a stable order.
    fn keyed(&self) -> BTreeMap<RouteKey<'_>, &RouteEntry> {
        self.hosts
            .iter()
            .flat_map(|(host, routes)| {
                routes.iter().map(move |entry| {
                    let key = RouteKey {
                        host,
                        prefix: &entry.prefix_str,
                    };
                    (key, entry)
                })
            })
            .collect()
    }

    /// Request host → table key: lowercase, no port, no trailing dot. The proxy compares a
    /// TLS SNI name against the request host in this form too (ADR-0032).
    pub fn normalize_host(raw: &str) -> Cow<'_, str> {
        let host = raw.trim();
        let host = match host.strip_prefix('[') {
            // An IPv6 literal never matches a route, but must not be cut at its colons.
            Some(rest) => rest.split(']').next().unwrap_or_default(),
            None => match host.rsplit_once(':') {
                Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
                _ => host,
            },
        };
        let host = host.strip_suffix('.').unwrap_or(host);
        if host.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Owned(host.to_ascii_lowercase())
        } else {
            Cow::Borrowed(host)
        }
    }
}

impl RouteEntry {
    /// Whether the prefix matches the leading whole segments of `path` (no query).
    ///
    /// Prefixes contain no `%`, `?`, empty or dot segments, so comparing the text up to a `/`
    /// boundary is the same as comparing segments, and `%2F` is never a separator.
    fn matches(&self, path: &str) -> bool {
        if self.prefix.is_root() {
            return true;
        }
        path.strip_prefix(self.prefix_str.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }

    /// The path and query to send upstream for the request target `path_and_query`, which this
    /// route matched (ADR-0010). Without `strip_prefix` it is unchanged. With it, the prefix
    /// segments are removed, the query kept, and an empty path becomes `/`:
    /// `/api/users?x=1` → `/users?x=1`, `/api` → `/`, `/api?x` → `/?x`.
    pub fn upstream_path<'a>(&self, path_and_query: &'a str) -> Cow<'a, str> {
        if !self.strip_prefix || self.prefix.is_root() {
            return Cow::Borrowed(path_and_query);
        }
        let split = path_and_query.find('?').unwrap_or(path_and_query.len());
        let (path, query) = path_and_query.split_at(split);
        if !self.matches(path) {
            return Cow::Borrowed(path_and_query);
        }
        let rest = &path[self.prefix_str.len()..];
        if rest.is_empty() {
            Cow::Owned(format!("/{query}"))
        } else {
            Cow::Borrowed(&path_and_query[self.prefix_str.len()..])
        }
    }

    /// The `X-Forwarded-Prefix` value: the matched prefix, or `None` for the root (§6.3).
    pub fn forwarded_prefix(&self) -> Option<&str> {
        (!self.prefix.is_root()).then_some(self.prefix_str.as_str())
    }

    fn same_as(&self, other: &RouteEntry) -> bool {
        self.owner == other.owner
            && self.strip_prefix == other.strip_prefix
            && self.timeouts == other.timeouts
            && self.target.same_as(&other.target)
    }
}

impl Target {
    /// Same destination: the very same upstream, or the same gap.
    fn same_as(&self, other: &Target) -> bool {
        match (self, other) {
            (Target::Loopback(a), Target::Loopback(b)) => Arc::ptr_eq(a, b),
            (Target::Unix(a), Target::Unix(b)) => a == b,
            (Target::Unavailable { retry_after: a }, Target::Unavailable { retry_after: b }) => {
                a == b
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RouteKey<'a> {
    host: &'a str,
    prefix: &'a str,
}

impl fmt::Display for RouteKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.host, self.prefix)
    }
}

/// The routes that differ between two tables, as `host/prefix` strings in sorted order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteDiff {
    pub from: u64,
    pub to: u64,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Same (host, prefix), but a new target, owner, strip setting or timeouts.
    pub changed: Vec<String>,
}

impl RouteDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

impl fmt::Display for RouteDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "route table v{} -> v{}: {} added, {} removed, {} changed",
            self.from,
            self.to,
            self.added.len(),
            self.removed.len(),
            self.changed.len()
        )?;
        for (sign, routes) in [
            ("+", &self.added),
            ("-", &self.removed),
            ("~", &self.changed),
        ] {
            routes
                .iter()
                .try_for_each(|route| write!(f, " {sign}{route}"))?;
        }
        Ok(())
    }
}
