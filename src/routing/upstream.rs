//! Where a route sends its requests: a container's loopback port or hoster's own Unix socket.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
        self.0.inflight.fetch_sub(1, Ordering::AcqRel);
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
