//! Which client connections the proxy takes on (ADR-0065): hoster's descriptor limit, and the
//! total cap that refuses new clients before the process runs out of descriptors.
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
//! A refused connection is closed at once and counted; while refusing, one warning goes to the
//! log every [`Admission::LOG_EVERY`]. The System page shows the count ([`FileUse`]).

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use pingora::listeners::ConnectionFilter;
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
/// process has [`Admission::cap`] descriptors open, counting the new connection's own. A cheap
/// handle: clones share the count.
#[derive(Clone)]
pub struct Admission(Arc<Inner>);

struct Inner {
    /// Fixed but in tests, which set it once they know what their harness has open.
    cap: AtomicUsize,
    files: OpenFiles,
    count: Mutex<Count>,
    refused: AtomicU64,
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

    /// Whether a connection accepted at `now` may stay.
    fn admit(&self, now: Instant) -> bool {
        let inner = &self.0;
        let mut count = inner.count.lock().unwrap_or_else(PoisonError::into_inner);
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
        let refused = inner.refused.fetch_add(1, Ordering::Relaxed) + 1;
        if count
            .logged
            .is_none_or(|at| now.saturating_duration_since(at) >= Self::LOG_EVERY)
        {
            count.logged = Some(now);
            tracing::warn!(
                open = estimate,
                cap,
                refused,
                "refusing new client connections: too many open files (ADR-0065)"
            );
        }
        false
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
    async fn should_accept(&self, _addr: Option<&std::net::SocketAddr>) -> bool {
        self.admit(Instant::now())
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
        assert!(admission.admit(now));
        assert!(admission.admit(now));
        assert!(admission.admit(now));
        assert!(!admission.admit(now));
        assert!(!admission.admit(now + Admission::REFRESH / 2));
        assert_eq!(admission.refused(), 2);
    }

    #[test]
    fn closed_connections_free_their_slot_at_the_next_refresh() {
        let (admission, answer) = fixed(10);
        let now = Instant::now();
        assert!(!admission.admit(now));
        *answer.lock().unwrap() = Ok(5);
        assert!(!admission.admit(now + Admission::REFRESH / 2), "not yet");
        assert!(admission.admit(now + Admission::REFRESH));
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
        assert!(!admission.admit(now));

        *answer.lock().unwrap() = Err(io::ErrorKind::NotFound.into());
        assert!(admission.admit(now + Admission::REFRESH));
        assert!(admission.admit(now + Admission::REFRESH));
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
