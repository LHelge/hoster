//! Where a route sends its requests: a container's loopback port or hoster's own Unix socket.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use crate::stack::Name;

/// One published container port of one deploy generation: `127.0.0.1:<allocated>` (ADR-0007).
///
/// Held as `Arc<Upstream>` by every `RouteTable` that routes to it, by in-flight requests and by
/// the deploy task that replaces it. The deploy task watches [`inflight`](Self::inflight) reach
/// zero after the swap to know the drain is done (architecture §3.2, §5.3).
#[derive(Debug)]
pub struct Upstream {
    pub addr: SocketAddr,
    pub stack: Name,
    pub service: Name,
    pub generation: u32,
    inflight: AtomicU64,
    /// Woken whenever `inflight` drops to zero, for [`drain`](Self::drain).
    idle: Notify,
}

/// How [`Upstream::drain`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// No request was in flight any more.
    Drained,
    /// The timeout passed with this many requests still in flight.
    TimedOut(u64),
}

impl Upstream {
    /// An upstream on `127.0.0.1:<port>` with no requests in flight.
    pub fn loopback(port: u16, stack: Name, service: Name, generation: u32) -> Self {
        Self {
            addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            stack,
            service,
            generation,
            inflight: AtomicU64::new(0),
            idle: Notify::new(),
        }
    }

    /// Requests currently proxied to this upstream.
    pub fn inflight(&self) -> u64 {
        self.inflight.load(Ordering::Acquire)
    }

    /// Counts one request against this upstream until the guard is dropped, so cancelled and
    /// failed requests are released too. The proxy keeps the guard in its request context.
    pub fn track(self: &Arc<Self>) -> InflightGuard {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        InflightGuard(Arc::clone(self))
    }

    /// Waits for this upstream to finish its in-flight requests after it was swapped out of the
    /// route table (architecture §5.3).
    ///
    /// Waits `settle` first, so a request that loaded the old table just before the swap but has
    /// not yet called [`track`](Self::track) is counted, then until no request is in flight.
    /// `timeout` bounds the whole call, settle delay included (the stack's `drain_timeout`); a
    /// settle delay longer than `timeout` is cut short.
    pub async fn drain(&self, settle: Duration, timeout: Duration) -> DrainOutcome {
        // `timeout` comes from the stack file and may be too large to add to an `Instant`, so the
        // remaining budget is a duration, which `tokio::time::timeout` clamps for us.
        let started = Instant::now();
        tokio::time::sleep(settle.min(timeout)).await;
        let left = timeout.saturating_sub(started.elapsed());
        let idle = async {
            loop {
                let notified = self.idle.notified();
                tokio::pin!(notified);
                // Registered before the check, so a drop to zero in between is not missed.
                notified.as_mut().enable();
                if self.inflight() == 0 {
                    return;
                }
                notified.await;
            }
        };
        match tokio::time::timeout(left, idle).await {
            Ok(()) => DrainOutcome::Drained,
            Err(_) => match self.inflight() {
                0 => DrainOutcome::Drained,
                left => DrainOutcome::TimedOut(left),
            },
        }
    }
}

/// One request in flight to an [`Upstream`]; see [`Upstream::track`].
#[derive(Debug)]
#[must_use = "the request is only counted while the guard is alive"]
pub struct InflightGuard(Arc<Upstream>);

impl InflightGuard {
    pub fn upstream(&self) -> &Arc<Upstream> {
        &self.0
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.0.inflight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

/// hoster's own app (admin UI and webhooks) on its Unix socket (ADR-0031).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixUpstream {
    path: PathBuf,
}

impl From<PathBuf> for UnixUpstream {
    fn from(path: PathBuf) -> Self {
        Self { path }
    }
}

impl AsRef<Path> for UnixUpstream {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}
