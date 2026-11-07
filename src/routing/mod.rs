//! `RouteTable`: host + longest segment-boundary prefix matching (ADR-0009) and the cross-stack
//! (host, prefix) collision check (ADR-0012).
//!
//! Pure: no I/O, so it carries most of the unit tests.
//!
//! [`RouteClaims::check`] validates one candidate stack against the others and the [`Reserved`]
//! hosts before a deploy or apply starts anything; [`RouteClaims::check_all`] is the same check
//! over a whole desired state, run again by [`RouteTable::build`].
//!
//! [`RouteTable::build`] turns a [`DesiredState`] (each stack's definition and what each service
//! runs) into an immutable table the proxy reads through `ArcSwap` (architecture §6). A route
//! leads to a [`Target`]: a container's loopback [`Upstream`], hoster's admin app on its
//! [`UnixUpstream`] (injected for the reserved admin host), or `Unavailable` while a stop-first
//! service is in its gap.

// The orchestrator and the proxy that consume the table land in later tasks.
#![allow(dead_code)]

mod collision;
mod desired;
mod table;
mod upstream;

#[allow(
    unused_imports,
    reason = "consumed by stack validation, the orchestrator and the proxy"
)]
pub use collision::{Collision, CollisionError, Owner, Reserved, RouteClaims, RouteOwner};
#[allow(unused_imports, reason = "consumed by the orchestrator and the proxy")]
pub use desired::{DesiredStack, DesiredState, ServiceState};
#[allow(unused_imports, reason = "consumed by the orchestrator and the proxy")]
pub use table::{RouteDiff, RouteEntry, RouteTable, Target};
#[allow(unused_imports, reason = "consumed by the orchestrator and the proxy")]
pub use upstream::{DrainOutcome, InflightGuard, UnixUpstream, Upstream};

#[cfg(test)]
mod table_tests;
#[cfg(test)]
mod tests;
