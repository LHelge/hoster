//! `RouteTable`: host + longest segment-boundary prefix matching (ADR-0009) and the cross-stack
//! (host, prefix) collision check (ADR-0012).
//!
//! Pure: no I/O, so it carries most of the unit tests.
//!
//! [`RouteClaims::check`] validates one candidate stack against the others and the [`Reserved`]
//! hosts before a deploy or apply starts anything; [`RouteClaims::check_all`] is the same check
//! over a whole desired state, run again by `RouteTable::build`.

// The orchestrator and the route table that consume the check land in later tasks.
#![allow(dead_code)]

mod collision;

#[allow(
    unused_imports,
    reason = "used by stack validation and RouteTable::build"
)]
pub use collision::{Collision, CollisionError, Owner, Reserved, RouteClaims, RouteOwner};

#[cfg(test)]
mod tests;
