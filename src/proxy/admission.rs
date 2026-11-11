//! Which client connections the proxy takes on (ADR-0065): hoster's descriptor limit, the
//! total cap that refuses new clients before the process runs out of descriptors, and the cap
//! per client address that keeps one client from taking every slot.
//!
//! [`FileLimit::raise`] lifts the soft `RLIMIT_NOFILE` to [`FileLimit::WANTED`] at startup (or
//! to the hard limit, if that is lower), so a self-updated binary gets it without a new unit.
//!
//! [`Admission`] is the `ConnectionFilter` on both listeners. Pingora asks it on every accept,
//! before TLS, and has no callback when a connection closes, so it does not count connections:
//! it counts what actually runs out, the descriptors in `/proc/self/fd`. Reading that directory
//! costs a few hundred microseconds with thousands open, so the count is taken at most every
//! [`Admission::REFRESH`] and every connection accepted since is added to it. The estimate
//! never falls below the real count by more than the descriptors the rest of the process opened
//! within one refresh, and a closed connection frees its slot at the next refresh.
//!
//! The per-address cap does count connections: at most [`Admission::PER_ADDRESS`] per IPv4
//! address or IPv6 /64 ([`Key`]). Each accepted connection takes a [`Slot`], attached to its
//! L4 stream as Pingora's `tracer`, whose `on_disconnected` runs when Pingora drops the stream
//! however the connection ended (`docs/spikes/per-address-connections.md`). Pingora never sets
//! that field on accepted streams. The slot is attached before the TLS handshake on `:443`
//! (Admission is the listener's `PreTlsProcess`), so a stalled or failed handshake holds and
//! frees one too, and on `:80`, which has no such callback, by [`Claiming`] around the app. The
//! filter only checks the count, so a burst from one address can get a few connections past it
//! before their slots are taken; counting in the filter instead would leak a slot for good
//! whenever Pingora drops a stream in between.
//!
//! A refused connection is closed at once and counted, whichever cap refused it; while
//! refusing, one warning goes to the log every [`Admission::LOG_EVERY`]. The System page shows
//! the count ([`FileUse`]).

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pingora::apps::ServerApp;
use pingora::listeners::{ConnectionFilter, PreTlsProcess};
use pingora::protocols::l4::stream::Stream as L4Stream;
use pingora::protocols::{GetSocketDigest, Stream};
use pingora::server::ShutdownWatch;
use pingora::upstreams::peer::{Tracer, Tracing};
use rustix::process::{Resource, Rlimit};

/// hoster's `RLIMIT_NOFILE`. `None` stands for unlimited, as in `getrlimit(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileLimit {
    /// The limit that applies, which the process may raise up to `hard`.
    pub soft: Option<u64>,
    pub hard: Option<u64>,
}

impl FileLimit {
    /// The soft limit hoster raises itself to (the unit's `LimitNOFILE=` too).
    pub const WANTED: u64 = 16_384;
    /// The open descriptors at which new client connections are refused.
    pub const CAP: usize = 4096;

    /// The limit as it is now.
    pub fn current() -> Self {
        rustix::process::getrlimit(Resource::Nofile).into()
    }

    /// Raises the soft limit to [`Self::WANTED`], or to the hard limit if that is lower; a soft
    /// limit already above it is kept. Returns the limit that applies afterwards, or the error
    /// with the limit left as it was.
    pub fn raise() -> Result<Self, (io::Error, Self)> {
        let limit = Self::current();
        let wanted = limit.raised();
        if wanted == limit {
            return Ok(limit);
        }
        match rustix::process::setrlimit(Resource::Nofile, wanted.into()) {
            Ok(()) => Ok(Self::current()),
            Err(err) => Err((err.into(), limit)),
        }
    }

    /// This limit with the soft limit raised as [`Self::raise`] does.
    fn raised(self) -> Self {
        let wanted = match self.hard {
            Some(hard) => Self::WANTED.min(hard),
            None => Self::WANTED,
        };
        match self.soft {
            Some(soft) if soft < wanted => Self {
                soft: Some(wanted),
                ..self
            },
            _ => self,
        }
    }

    /// The open descriptors at which new client connections are refused: [`Self::CAP`], or a
    /// quarter of the soft limit if the hard limit kept that low.
    pub fn connection_cap(self) -> usize {
        match self.soft {
            Some(soft) => usize::try_from(soft / 4).map_or(Self::CAP, |q| q.min(Self::CAP)),
            None => Self::CAP,
        }
    }
}

impl From<Rlimit> for FileLimit {
    fn from(limit: Rlimit) -> Self {
        Self {
            soft: limit.current,
            hard: limit.maximum,
        }
    }
}

impl From<FileLimit> for Rlimit {
    fn from(limit: FileLimit) -> Self {
        Self {
            current: limit.soft,
            maximum: limit.hard,
        }
    }
}

impl fmt::Display for FileLimit {
    /// `16384 (hard 524288)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unlimited =
            |limit: Option<u64>| limit.map_or("unlimited".to_owned(), |l| l.to_string());
        write!(
            f,
            "{} (hard {})",
            unlimited(self.soft),
            unlimited(self.hard)
        )
    }
}

/// How hoster's descriptors are used, as the System page shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileUse {
    /// Open now; `None` when `/proc/self/fd` cannot be read.
    pub open: Option<usize>,
    pub limit: FileLimit,
    /// Where new client connections are refused.
    pub cap: usize,
    /// Client connections refused since hoster started.
    pub refused: u64,
}

/// The `ConnectionFilter` of the proxy's listeners: refuses a new client connection while the
/// process has [`Admission::cap`] descriptors open, counting the new connection's own, or its
/// address has [`Admission::PER_ADDRESS`] connections open. A cheap handle: clones share the
/// counts.
#[derive(Clone)]
pub struct Admission(Arc<Inner>);

struct Inner {
    /// Fixed but in tests, which set it once they know what their harness has open.
    cap: AtomicUsize,
    files: OpenFiles,
    count: Mutex<Count>,
    refused: AtomicU64,
    /// The open connections per client; a key leaves the map when its count reaches zero, so
    /// the map holds at most one entry per open connection.
    addresses: Mutex<HashMap<Key, usize>>,
}

/// The descriptor count as last measured, and what was let in since.
#[derive(Debug, Default)]
struct Count {
    /// When the count was last taken.
    measured: Option<Instant>,
    /// What it was; `None` when it could not be taken, and then every connection is let in.
    open: Option<usize>,
    /// Connections accepted since.
    accepted: usize,
    /// When refusing was last logged.
    logged: Option<Instant>,
    /// Whether the count failing has been logged.
    unmeasurable: bool,
}

impl fmt::Debug for Admission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Admission")
            .field("cap", &self.cap())
            .field("refused", &self.refused())
            .finish_non_exhaustive()
    }
}

impl From<FileLimit> for Admission {
    /// Refusing at the limit's [`FileLimit::connection_cap`].
    fn from(limit: FileLimit) -> Self {
        Self::new(limit.connection_cap())
    }
}

impl Default for Admission {
    /// Refusing at the cap for the limit that applies now.
    fn default() -> Self {
        FileLimit::current().into()
    }
}

impl Admission {
    /// How old the descriptor count may get.
    pub const REFRESH: Duration = Duration::from_millis(100);
    /// How often refusing is logged while it lasts.
    pub const LOG_EVERY: Duration = Duration::from_secs(10);
    /// The open connections one client address (an IPv4 address, an IPv6 /64) may have.
    pub const PER_ADDRESS: usize = 256;

    /// Refusing new clients at `cap` open descriptors.
    pub fn new(cap: usize) -> Self {
        Self::counting(cap, OpenFiles::Proc)
    }

    fn counting(cap: usize, files: OpenFiles) -> Self {
        Self(Arc::new(Inner {
            cap: AtomicUsize::new(cap),
            files,
            count: Mutex::default(),
            refused: AtomicU64::new(0),
            addresses: Mutex::default(),
        }))
    }

    /// The open descriptors at which new client connections are refused.
    pub fn cap(&self) -> usize {
        self.0.cap.load(Ordering::Relaxed)
    }

    /// Moves the cap to `cap`.
    #[cfg(test)]
    pub(crate) fn set_cap(&self, cap: usize) {
        self.0.cap.store(cap, Ordering::Relaxed);
    }

    /// The client connections refused so far.
    pub fn refused(&self) -> u64 {
        self.0.refused.load(Ordering::Relaxed)
    }

    /// The descriptors in use now, counted afresh.
    pub fn usage(&self) -> FileUse {
        FileUse {
            open: self.0.files.count().ok(),
            limit: FileLimit::current(),
            cap: self.cap(),
            refused: self.refused(),
        }
    }

    /// `app` taking a slot for each plain connection it is handed (`:80`).
    pub(super) fn claiming<A>(&self, app: A) -> Claiming<A> {
        Claiming {
            app: Arc::new(app),
            admission: self.clone(),
        }
    }

    /// The connections open from `key`'s address.
    fn open_from(&self, key: Key) -> usize {
        let addresses = self
            .0
            .addresses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        addresses.get(&key).copied().unwrap_or(0)
    }

    /// Whether a connection from `peer` accepted at `now` may stay.
    fn admit(&self, now: Instant, peer: Option<IpAddr>) -> bool {
        let crowded = peer
            .map(Key::from)
            .filter(|&key| self.open_from(key) >= Self::PER_ADDRESS);
        let inner = &self.0;
        let mut count = inner.count.lock().unwrap_or_else(PoisonError::into_inner);
        let refusal = match crowded {
            Some(key) => Refusal::Address(key),
            None => {
                let stale = count
                    .measured
                    .is_none_or(|at| now.saturating_duration_since(at) >= Self::REFRESH);
                if stale {
                    count.measure(&inner.files, now);
                }
                let Some(open) = count.open else {
                    return true;
                };
                let estimate = open.saturating_add(count.accepted);
                let cap = self.cap();
                if estimate < cap {
                    count.accepted += 1;
                    return true;
                }
                Refusal::Total {
                    open: estimate,
                    cap,
                }
            }
        };
        let refused = inner.refused.fetch_add(1, Ordering::Relaxed) + 1;
        if count
            .logged
            .is_none_or(|at| now.saturating_duration_since(at) >= Self::LOG_EVERY)
        {
            count.logged = Some(now);
            tracing::warn!(
                refused,
                "refusing new client connections: {refusal} (ADR-0065)"
            );
        }
        false
    }

    /// Takes a slot for `stream`'s peer, unless it holds one already or has no address.
    fn claim(&self, stream: &mut L4Stream) {
        if stream.tracer.is_some() {
            return;
        }
        let peer = stream
            .get_socket_digest()
            .and_then(|digest| digest.peer_addr().and_then(|addr| addr.as_inet()).copied());
        if let Some(peer) = peer {
            stream.tracer = Some(Tracer(Box::new(self.take(Key::from(peer.ip())))));
        }
    }

    /// [`Admission::claim`] for a stream as Pingora hands it to an app: a plain connection is
    /// the L4 stream itself, boxed.
    fn claim_plain(&self, stream: Stream) -> Stream {
        if !stream.as_any().is::<L4Stream>() {
            return stream;
        }
        let Ok(mut l4) = stream.into_any().downcast::<L4Stream>() else {
            unreachable!("the stream was checked to be an L4 stream");
        };
        self.claim(&mut l4);
        l4
    }

    /// Counts one more connection from `key`.
    fn take(&self, key: Key) -> Slot {
        let mut addresses = self
            .0
            .addresses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *addresses.entry(key).or_default() += 1;
        Slot {
            admission: Some(self.clone()),
            key,
        }
    }

    fn release(&self, key: Key) {
        let mut addresses = self
            .0
            .addresses
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(open) = addresses.get_mut(&key) {
            *open -= 1;
            if *open == 0 {
                addresses.remove(&key);
            }
        }
    }
}

#[cfg(test)]
impl Admission {
    /// The connections open from `ip`'s address (its /64 for IPv6).
    pub(crate) fn open_from_ip(&self, ip: IpAddr) -> usize {
        self.open_from(Key::from(ip))
    }

    /// The addresses with connections open.
    pub(crate) fn clients(&self) -> usize {
        self.0.addresses.lock().unwrap().len()
    }
}

/// Why a connection was refused, as the log says it.
enum Refusal {
    Total { open: usize, cap: usize },
    Address(Key),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Total { open, cap } => {
                write!(f, "too many open files ({open}, cap {cap})")
            }
            Refusal::Address(key) => {
                write!(f, "{} connections open from {key}", Admission::PER_ADDRESS)
            }
        }
    }
}

/// What the per-address cap counts by: an IPv4 address, or the /64 of an IPv6 one. An
/// IPv4-mapped IPv6 address is the IPv4 address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key(IpAddr);

impl From<IpAddr> for Key {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => Key(v4.into()),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => Key(v4.into()),
                None => Key(Ipv6Addr::from(u128::from(v6) & !u128::from(u64::MAX)).into()),
            },
        }
    }
}

impl fmt::Display for Key {
    /// `203.0.113.9`, `2001:db8:1:2::/64`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            IpAddr::V4(v4) => v4.fmt(f),
            IpAddr::V6(v6) => write!(f, "{v6}/64"),
        }
    }
}

/// One connection's place under the per-address cap, held as its L4 stream's `tracer` and
/// given back when Pingora drops the stream.
#[derive(Debug)]
struct Slot {
    /// `None` in a clone, which must not give the slot back a second time.
    admission: Option<Admission>,
    key: Key,
}

impl Tracing for Slot {
    fn on_connected(&self) {}

    fn on_disconnected(&self) {
        if let Some(admission) = &self.admission {
            admission.release(self.key);
        }
    }

    fn boxed_clone(&self) -> Box<dyn Tracing> {
        Box::new(Slot {
            admission: None,
            key: self.key,
        })
    }
}

/// An app that takes a slot for each plain connection before handling it, for the listener
/// without TLS, where no pre-TLS callback runs. Built by [`Admission::claiming`].
pub(super) struct Claiming<A> {
    app: Arc<A>,
    admission: Admission,
}

#[async_trait]
impl<A: ServerApp + Send + Sync + 'static> ServerApp for Claiming<A> {
    async fn process_new(
        self: &Arc<Self>,
        stream: Stream,
        shutdown: &ShutdownWatch,
    ) -> Option<Stream> {
        // A kept-alive connection comes back here with its slot already taken.
        let stream = self.admission.claim_plain(stream);
        self.app.process_new(stream, shutdown).await
    }

    async fn cleanup(&self) {
        self.app.cleanup().await;
    }
}

impl Count {
    fn measure(&mut self, files: &OpenFiles, now: Instant) {
        self.measured = Some(now);
        self.accepted = 0;
        self.open = match files.count() {
            Ok(open) => Some(open),
            // Out of descriptors: certainly at the cap.
            Err(err) if OpenFiles::exhausted(&err) => Some(usize::MAX),
            Err(err) => {
                if !std::mem::replace(&mut self.unmeasurable, true) {
                    tracing::warn!(
                        error = %err,
                        "cannot count open files; client connections are not capped"
                    );
                }
                None
            }
        };
    }
}

#[async_trait]
impl ConnectionFilter for Admission {
    async fn should_accept(&self, addr: Option<&SocketAddr>) -> bool {
        self.admit(Instant::now(), addr.map(SocketAddr::ip))
    }
}

#[async_trait]
impl PreTlsProcess for Admission {
    /// Takes the connection's slot before its TLS handshake (`:443`).
    async fn process(&self, stream: &mut L4Stream) -> pingora::Result<()> {
        self.claim(stream);
        Ok(())
    }
}

/// Where the open descriptors are counted.
enum OpenFiles {
    /// `/proc/self/fd`.
    Proc,
    /// A fixed answer (tests).
    #[cfg(test)]
    Fixed(Arc<Mutex<io::Result<usize>>>),
}

impl OpenFiles {
    fn count(&self) -> io::Result<usize> {
        match self {
            OpenFiles::Proc => Self::count_in(Path::new("/proc/self/fd")),
            #[cfg(test)]
            OpenFiles::Fixed(answer) => match &*answer.lock().unwrap() {
                Ok(open) => Ok(*open),
                Err(err) => Err(err
                    .raw_os_error()
                    .map_or_else(|| err.kind().into(), io::Error::from_raw_os_error)),
            },
        }
    }

    /// The entries of `dir`, less the descriptor reading it holds.
    fn count_in(dir: &Path) -> io::Result<usize> {
        let mut open = 0usize;
        for entry in std::fs::read_dir(dir)? {
            entry?;
            open += 1;
        }
        Ok(open.saturating_sub(1))
    }

    /// Whether `err` says the process or the system is out of descriptors.
    fn exhausted(err: &io::Error) -> bool {
        let errno = err.raw_os_error();
        errno == Some(rustix::io::Errno::MFILE.raw_os_error())
            || errno == Some(rustix::io::Errno::NFILE.raw_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(open: usize) -> (Admission, Arc<Mutex<io::Result<usize>>>) {
        let answer = Arc::new(Mutex::new(Ok(open)));
        (
            Admission::counting(10, OpenFiles::Fixed(Arc::clone(&answer))),
            answer,
        )
    }

    #[test]
    fn the_soft_limit_is_raised_to_the_wanted_one_or_the_hard_limit() {
        let limit = |soft, hard| FileLimit { soft, hard };
        let systemd = limit(Some(1024), Some(524_288));
        assert_eq!(systemd.raised(), limit(Some(16_384), Some(524_288)));
        assert_eq!(limit(Some(1024), Some(4096)).raised().soft, Some(4096));
        assert_eq!(limit(Some(1024), None).raised().soft, Some(16_384));
        // Never lowered.
        assert_eq!(limit(Some(65_536), None).raised().soft, Some(65_536));
        assert_eq!(limit(None, None).raised().soft, None);
    }

    #[test]
    fn the_cap_is_a_quarter_of_a_low_limit() {
        let soft = |soft| FileLimit { soft, hard: soft };
        assert_eq!(soft(Some(16_384)).connection_cap(), 4096);
        assert_eq!(soft(Some(524_288)).connection_cap(), 4096);
        assert_eq!(soft(Some(4096)).connection_cap(), 1024);
        assert_eq!(soft(Some(128)).connection_cap(), 32);
        assert_eq!(soft(None).connection_cap(), 4096);
        assert_eq!(soft(Some(1024)).to_string(), "1024 (hard 1024)", "Display");
    }

    #[test]
    fn raising_twice_keeps_the_limit() {
        let first = FileLimit::raise().unwrap_or_else(|(_, limit)| limit);
        let second = FileLimit::raise().unwrap_or_else(|(_, limit)| limit);
        assert_eq!(first, second);
        assert_eq!(first, FileLimit::current());
    }

    #[test]
    fn connections_are_refused_at_the_cap_and_counted() {
        let (admission, _) = fixed(7);
        let now = Instant::now();
        // 7 open, cap 10: three more, counted between refreshes.
        assert!(admission.admit(now, None));
        assert!(admission.admit(now, None));
        assert!(admission.admit(now, None));
        assert!(!admission.admit(now, None));
        assert!(!admission.admit(now + Admission::REFRESH / 2, None));
        assert_eq!(admission.refused(), 2);
    }

    #[test]
    fn closed_connections_free_their_slot_at_the_next_refresh() {
        let (admission, answer) = fixed(10);
        let now = Instant::now();
        assert!(!admission.admit(now, None));
        *answer.lock().unwrap() = Ok(5);
        assert!(
            !admission.admit(now + Admission::REFRESH / 2, None),
            "not yet"
        );
        assert!(admission.admit(now + Admission::REFRESH, None));
        assert_eq!(admission.refused(), 2);
    }

    #[test]
    fn running_out_of_descriptors_refuses_and_an_unreadable_count_does_not() {
        let (admission, answer) = fixed(0);
        *answer.lock().unwrap() = Err(io::Error::from_raw_os_error(
            rustix::io::Errno::MFILE.raw_os_error(),
        ));
        assert!(OpenFiles::exhausted(&io::Error::from_raw_os_error(
            rustix::io::Errno::NFILE.raw_os_error()
        )));
        let now = Instant::now();
        assert!(!admission.admit(now, None));

        *answer.lock().unwrap() = Err(io::ErrorKind::NotFound.into());
        assert!(admission.admit(now + Admission::REFRESH, None));
        assert!(admission.admit(now + Admission::REFRESH, None));
    }

    #[test]
    fn keys_are_ipv4_addresses_and_ipv6_slash_64s() {
        let key = |ip: &str| Key::from(ip.parse::<IpAddr>().unwrap());
        assert_eq!(key("192.0.2.1"), key("::ffff:192.0.2.1"));
        assert_ne!(key("192.0.2.1"), key("192.0.2.2"));
        assert_eq!(key("2001:db8:1:2::1"), key("2001:db8:1:2:ffff::9"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
        assert_eq!(key("2001:db8:1:2:3::4").to_string(), "2001:db8:1:2::/64");
        assert_eq!(key("::ffff:192.0.2.1").to_string(), "192.0.2.1");
    }

    #[test]
    fn an_address_is_refused_at_its_cap_and_one_slot_back_lets_it_in() {
        let (admission, _) = fixed(0);
        admission.set_cap(usize::MAX);
        let now = Instant::now();
        let ip = |ip: &str| Some(ip.parse::<IpAddr>().unwrap());
        // Two addresses of one /64 share the budget.
        let mut slots: Vec<Slot> = (0..Admission::PER_ADDRESS)
            .map(|i| {
                let peer = if i % 2 == 0 {
                    "2001:db8::1"
                } else {
                    "2001:db8::2"
                };
                assert!(admission.admit(now, ip(peer)));
                admission.take(Key::from(ip(peer).unwrap()))
            })
            .collect();
        assert!(!admission.admit(now, ip("2001:db8::3")));
        assert!(admission.admit(now, ip("2001:db8:0:1::1")), "another /64");
        assert!(admission.admit(now, ip("192.0.2.1")));
        assert!(admission.admit(now, None));
        assert_eq!(admission.refused(), 1);

        slots.pop().unwrap().on_disconnected();
        assert!(admission.admit(now, ip("2001:db8::3")));
        // A clone gives nothing back.
        Tracer(Box::new(slots.pop().unwrap()))
            .clone()
            .0
            .on_disconnected();
        assert_eq!(
            admission.open_from(Key::from(ip("2001:db8::1").unwrap())),
            255
        );
        for slot in &slots {
            slot.on_disconnected();
        }
        assert_eq!(
            admission.open_from(Key::from(ip("2001:db8::1").unwrap())),
            1
        );
        assert_eq!(admission.0.addresses.lock().unwrap().len(), 1);
    }

    #[test]
    fn proc_counts_this_process_without_the_reading_descriptor() {
        let before = OpenFiles::Proc.count().unwrap();
        let held: Vec<_> = (0..5).map(|_| tempfile::tempfile().unwrap()).collect();
        let after = OpenFiles::Proc.count().unwrap();
        // Other test threads may open files meanwhile.
        assert!(after >= before + held.len(), "{before} → {after}");
        let usage = Admission::new(4096).usage();
        assert!(
            usage.open.is_some_and(|open| open >= held.len()),
            "{usage:?}"
        );
        assert_eq!(usage.limit, FileLimit::current());
    }
}
