//! Upstream port allocator (ADR-0026, ADR-0007).
//!
//! Routed containers publish their ports on `127.0.0.1` at host ports hoster chooses from the
//! configured [`PortRange`]. [`PortAllocator`] hands them out round-robin: the next free port
//! after the last one handed out, wrapping around, so a released port is not reused right away.
//! Allocations and the cursor live in the state store and survive a restart; each claim runs in
//! one store transaction, so concurrent allocations never get the same port.
//!
//! A port that podman could not bind ("address in use": some other host process holds it) is
//! [marked bad](PortAllocator::mark_bad) and skipped for [`PortAllocator::QUARANTINE`]. Bad marks
//! are kept in memory only; after a restart the bind is simply tried again.

#![allow(dead_code, reason = "the allocator is consumed by the orchestrator")]

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::config::PortRange;
use crate::store::{PortAllocation, PortRequest, Store, StoreError};

/// Errors from the port allocator.
#[derive(Debug, Error)]
pub enum PortError {
    /// Every port in the range is allocated or marked bad. Deploy validation turns this into a
    /// `Rejected` deployment (ADR-0026).
    #[error("no free upstream port in {range}: every port is allocated or in use on the host")]
    Exhausted { range: PortRange },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Hands out loopback host ports for routed containers. Cheap to share behind an `Arc`.
#[derive(Debug)]
pub struct PortAllocator {
    store: Store,
    range: PortRange,
    bad: Mutex<BadPorts>,
}

impl PortAllocator {
    /// How long a port that was in use on the host is skipped.
    pub const QUARANTINE: Duration = Duration::from_secs(10 * 60);

    pub fn new(store: Store, range: PortRange) -> Self {
        Self {
            store,
            range,
            bad: Mutex::default(),
        }
    }

    /// The range ports are allocated from.
    pub fn range(&self) -> PortRange {
        self.range
    }

    /// Allocates a fresh port for one container port of a service's deploy generation.
    pub async fn allocate(
        &self,
        stack: &str,
        service: &str,
        container_port: u16,
        generation: u32,
    ) -> Result<u16, PortError> {
        let request = PortRequest {
            stack: stack.to_owned(),
            service: service.to_owned(),
            container_port,
            generation,
        };
        let range = self.range;
        let allocation = self
            .store
            .claim_port(&request, |cursor, taken| {
                // Read the quarantine inside the claim, after `taken`: `mark_bad` marks a port
                // before releasing it, so a port this transaction sees as free is either not
                // bad or already marked here.
                let bad = self.bad_ports(Instant::now());
                RoundRobin { range, cursor }
                    .next_free(|port| taken.contains(&port) || bad.contains(&port))
            })
            .await?
            .ok_or(PortError::Exhausted { range })?;
        tracing::debug!(
            port = allocation.port,
            stack,
            service,
            container_port,
            generation,
            "allocated upstream port"
        );
        Ok(allocation.port)
    }

    /// Releases a port. Returns whether it was allocated.
    pub async fn release(&self, port: u16) -> Result<bool, PortError> {
        Ok(self.store.release_port(port).await?)
    }

    /// Every allocated port, in port order.
    pub async fn list(&self) -> Result<Vec<PortAllocation>, PortError> {
        Ok(self.store.port_allocations().await?)
    }

    /// Records that `port` is in use by some other process on the host: releases its allocation
    /// and skips it for [`Self::QUARANTINE`]. The caller then allocates again.
    ///
    /// The mark is set before the release so that a concurrent [`Self::allocate`] can never see
    /// the port free but not yet bad.
    pub async fn mark_bad(&self, port: u16) -> Result<(), PortError> {
        self.lock_bad().mark(port, Instant::now());
        tracing::warn!(
            port,
            quarantine = ?Self::QUARANTINE,
            "upstream port in use on the host; skipping it"
        );
        self.release(port).await?;
        Ok(())
    }

    fn bad_ports(&self, now: Instant) -> BTreeSet<u16> {
        self.lock_bad().current(now)
    }

    fn lock_bad(&self) -> std::sync::MutexGuard<'_, BadPorts> {
        // The map holds plain data; a panic while holding the lock cannot leave it inconsistent.
        self.bad.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The round-robin policy: walk the range once, starting just after the cursor and wrapping.
#[derive(Debug, Clone, Copy)]
struct RoundRobin {
    range: PortRange,
    /// The port last handed out. Outside the range (the range was reconfigured) or unset, the
    /// walk starts at the first port.
    cursor: Option<u16>,
}

impl RoundRobin {
    /// The first port of the walk for which `unavailable` is false.
    fn next_free(self, unavailable: impl Fn(u16) -> bool) -> Option<u16> {
        self.into_iter().find(|&port| !unavailable(port))
    }
}

impl IntoIterator for RoundRobin {
    type Item = u16;
    type IntoIter = std::iter::Take<
        std::iter::Chain<std::ops::RangeInclusive<u16>, std::ops::RangeInclusive<u16>>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        let (first, last) = (self.range.first(), self.range.last());
        let start = match self.cursor {
            Some(c) if self.range.contains(c) && c < last => c + 1,
            _ => first,
        };
        (start..=last).chain(first..=last).take(self.range.len())
    }
}

/// Ports found in use on the host, with when they were marked.
#[derive(Debug, Default)]
struct BadPorts(HashMap<u16, Instant>);

impl BadPorts {
    fn mark(&mut self, port: u16, now: Instant) {
        self.0.insert(port, now);
    }

    /// The ports still in quarantine at `now`; expired marks are dropped.
    fn current(&mut self, now: Instant) -> BTreeSet<u16> {
        self.0
            .retain(|_, marked| now.saturating_duration_since(*marked) < PortAllocator::QUARANTINE);
        self.0.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests;
