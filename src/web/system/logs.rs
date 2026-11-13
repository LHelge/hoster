//! The System page's log store card (ADR-0067 §2, §7): how large `logs.db` is against its
//! 1 GiB cap, how far back it reaches and whether the size cap or the 7 days decide that, what
//! it took in during the last hour, and what was lost since hoster started, by cause.
//!
//! [`LogFacts`] reads the figures when the page renders. [`LocalLogs`] reads them from the
//! store, the recorder's [`LogSink`] (channel full, failed commits, the last retention pass),
//! the follower registry (lines the per-container rate cap dropped) and [`HosterRecords`]
//! (hoster's own events the cap dropped). The counters live in memory and start at zero with
//! hoster; nothing here is persisted. When `logs.db` could not be opened at start, the card
//! says so with the error and the rest of the page is unaffected.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::host::Fact;
use crate::logging::HosterRecords;
use crate::logs::{Drops, LogFollowers, LogSink};
use crate::machine::Memory;
use crate::runtime::{ContainerId, ContainerRuntime};
use crate::store::{LogStore, Retention, RowCounts, SweepReport, Timestamp};
use crate::web::containers::ContainerSource;
use crate::web::ui::{Count, Load, Meter, Percent, Uptime};

/// Where the System page's log store figures come from.
#[async_trait::async_trait]
pub trait LogFacts: Send + Sync + 'static {
    /// The figures as they are now.
    async fn read(&self) -> LogHealth;
}

/// The log store as hoster opened it at start.
#[derive(Debug, Clone)]
pub enum LogStoreState {
    Open(LogStore),
    /// `logs.db` could not be opened, with why: hoster runs without persisted logs.
    Unavailable(String),
}

/// What the card shows.
#[derive(Debug, Clone, PartialEq)]
pub enum LogHealth {
    Open(Box<LogFigures>),
    /// `logs.db` could not be opened at start, with the error.
    Unavailable(String),
}

/// The figures of an open log store. A figure that could not be read is `None`, with the
/// first error in `problem`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LogFigures {
    pub path: Option<PathBuf>,
    /// The bytes the rows take, measured as the size cap measures them.
    pub used_bytes: Option<u64>,
    /// When the oldest stored record was stamped; `None` with no records (or unread).
    pub oldest: Option<SystemTime>,
    /// The containers whose output is being followed into the store.
    pub followed: usize,
    /// Records and request rows stamped in the last hour.
    pub last_hour: Option<RowCounts>,
    /// The last finished retention pass since hoster started.
    pub sweep: Option<SweepReport>,
    /// Lost to a full channel and to failed commits.
    pub drops: Drops,
    /// Lines the per-container rate cap dropped, by container name, most first.
    pub capped: Vec<(String, u64)>,
    /// hoster's own events the rate cap dropped.
    pub hoster_capped: u64,
    /// Why a figure is missing.
    pub problem: Option<String>,
}

impl LogFigures {
    /// The rate-capped containers shown by name; the rest are summed.
    const TOP_CAPPED: usize = 3;

    /// A store a week into its retention with some losses (tests and the UI preview).
    #[cfg(test)]
    pub fn sample(now: SystemTime) -> Self {
        use crate::store::Removed;
        Self {
            path: Some("/home/hoster/.local/state/hoster/logs.db".into()),
            used_bytes: Some(870 * 1024 * 1024),
            oldest: Some(now - Duration::from_secs(6 * 86_400 + 20 * 3_600)),
            followed: 9,
            last_hour: Some(RowCounts {
                records: 18_204,
                requests: 3_311,
                spans: 0,
            }),
            sweep: Some(SweepReport {
                started: Timestamp::from(now - Duration::from_secs(1_800)),
                expired: Removed {
                    records: 2_410,
                    requests: 512,
                    spans: 0,
                },
                capped: Removed::default(),
                file_bytes: 912 * 1024 * 1024,
            }),
            drops: Drops {
                records: 120,
                requests: 4,
                ..Drops::default()
            },
            capped: vec![
                ("notes-api-7".into(), 12_040),
                ("blog-app-3".into(), 310),
                ("shop-web-2".into(), 22),
                ("wiki-app-1".into(), 5),
            ],
            hoster_capped: 0,
            problem: None,
        }
    }

    /// The size against the cap: `870 MiB / 1.0 GiB`, `85%` and its meter.
    fn size(&self) -> Option<(String, String, Meter)> {
        let used = self.used_bytes?;
        #[expect(clippy::cast_precision_loss, reason = "only the share matters")]
        let load = Load::new(used as f64, Retention::MAX_BYTES as f64);
        let share = load.share().unwrap_or(0.0) * 100.0;
        let text = format!("{} / {}", Memory(used), Memory(Retention::MAX_BYTES));
        let meter = load.meter(format!("{text} of the size cap"));
        Some((text, format!("{} of the cap", Percent(share)), meter))
    }

    /// The rows under the meter.
    fn rows(&self, now: SystemTime) -> Vec<Fact> {
        let mut rows = vec![Fact {
            name: "file",
            value: self.path.as_ref().map(|p| p.display().to_string()),
            note: None,
        }];
        let read = self.problem.is_none() || self.last_hour.is_some();
        rows.push(match self.oldest {
            Some(oldest) => Fact {
                name: "oldest",
                value: Some(humantime::format_rfc3339_seconds(oldest).to_string()),
                note: Some(format!(
                    "{} ago",
                    Uptime(now.duration_since(oldest).unwrap_or_default())
                )),
            },
            None => Fact {
                name: "oldest",
                value: read.then(|| "no records yet".to_owned()),
                note: None,
            },
        });
        rows.push(Fact {
            name: "last hour",
            value: self.last_hour.map(|rows| {
                format!(
                    "{} records · {} requests",
                    Count(rows.records),
                    Count(rows.requests)
                )
            }),
            note: None,
        });
        rows.push(self.limit());
        rows.push(Fact {
            name: "following",
            value: Some(format!("{} containers", self.followed)),
            note: None,
        });
        rows
    }

    /// What decides the oldest record: the size cap when the last pass had to cut whole hours,
    /// else the 7 days.
    fn limit(&self) -> Fact {
        let days = Retention::MAX_AGE.as_secs() / 86_400;
        let (value, note) = match self.sweep {
            None => (
                format!("{days} days"),
                Some("no retention pass has finished since hoster started".to_owned()),
            ),
            Some(sweep) if sweep.capped.records + sweep.capped.requests > 0 => (
                format!("the {} size cap", Memory(Retention::MAX_BYTES)),
                Some(format!(
                    "the last pass removed {} records and {} requests over it, oldest hours first",
                    Count(sweep.capped.records),
                    Count(sweep.capped.requests)
                )),
            ),
            Some(sweep) => (
                format!("{days} days"),
                Some(format!(
                    "the last pass removed {} records and {} requests older than that",
                    Count(sweep.expired.records),
                    Count(sweep.expired.requests)
                )),
            ),
        };
        Fact {
            name: "kept for",
            value: Some(value),
            note,
        }
    }

    /// One row per cause that lost anything since hoster started.
    fn losses(&self) -> Vec<Fact> {
        let mut rows = Vec::new();
        let full = Self::parts(&[
            (self.drops.records, "container lines"),
            (self.drops.otlp_records, "OTLP records"),
            (self.drops.requests, "request rows"),
            (self.drops.spans, "spans"),
        ]);
        if let Some(full) = full {
            rows.push(Fact {
                name: "channel full",
                value: Some(full),
                note: Some("the recorder fell behind".into()),
            });
        }
        if self.drops.failed_commits > 0 {
            rows.push(Fact {
                name: "failed commits",
                value: Some(format!(
                    "{} commits · {} records and rows",
                    Count(self.drops.failed_commits),
                    Count(self.drops.lost_in_failed_commits)
                )),
                note: Some("hoster's journal has the errors".into()),
            });
        }
        if !self.capped.is_empty() {
            let mut shown: Vec<String> = self
                .capped
                .iter()
                .take(Self::TOP_CAPPED)
                .map(|(name, lines)| format!("{name} {}", Count(*lines)))
                .collect();
            let rest = &self.capped[self.capped.len().min(Self::TOP_CAPPED)..];
            if !rest.is_empty() {
                let lines: u64 = rest.iter().map(|(_, lines)| lines).sum();
                shown.push(format!("+{} more ({})", rest.len(), Count(lines)));
            }
            rows.push(Fact {
                name: "line-rate cap",
                value: Some(shown.join(" · ")),
                note: Some("lines over the per-container rate".into()),
            });
        }
        let hoster = Self::parts(&[
            (self.drops.hoster_records, "channel full"),
            (self.hoster_capped, "rate cap"),
        ]);
        if let Some(hoster) = hoster {
            rows.push(Fact {
                name: "hoster events",
                value: Some(hoster),
                note: None,
            });
        }
        rows
    }

    /// `120 container lines · 4 request rows`, leaving out the zeros; `None` when all are.
    fn parts(parts: &[(u64, &str)]) -> Option<String> {
        let parts: Vec<String> = parts
            .iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, what)| format!("{} {what}", Count(*n)))
            .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// The card as the template draws it.
pub struct LogCard {
    /// Why `logs.db` is not open.
    pub unavailable: Option<String>,
    /// `870 MiB / 1.0 GiB`, `85% of the cap` and its meter.
    pub size: Option<(String, String, Meter)>,
    pub rows: Vec<Fact>,
    /// Empty when nothing was lost.
    pub losses: Vec<Fact>,
    pub problem: Option<String>,
}

impl LogCard {
    /// The card for `health` as of `now`.
    pub fn new(health: LogHealth, now: SystemTime) -> Self {
        match health {
            LogHealth::Unavailable(error) => Self {
                unavailable: Some(error),
                size: None,
                rows: Vec::new(),
                losses: Vec::new(),
                problem: None,
            },
            LogHealth::Open(figures) => Self {
                unavailable: None,
                size: figures.size(),
                rows: figures.rows(now),
                losses: figures.losses(),
                problem: figures.problem,
            },
        }
    }
}

#[async_trait::async_trait]
impl LogFacts for LogHealth {
    /// Always these figures (tests and the UI preview).
    async fn read(&self) -> LogHealth {
        self.clone()
    }
}

/// What the follower registry counts: the containers it follows and what the per-container
/// rate cap dropped.
pub trait FollowerCounts: Send + Sync + 'static {
    /// How many containers are followed right now.
    fn followed(&self) -> usize;
    /// Lines dropped since hoster started, per container that lost any.
    fn capped(&self) -> BTreeMap<ContainerId, u64>;
}

impl<R: ContainerRuntime> FollowerCounts for LogFollowers<R> {
    fn followed(&self) -> usize {
        LogFollowers::followed(self).len()
    }

    fn capped(&self) -> BTreeMap<ContainerId, u64> {
        LogFollowers::capped(self)
    }
}

/// The log store of this hoster and the counters of its pipeline.
pub struct LocalLogs {
    state: LogStoreState,
    sink: LogSink,
    followers: Arc<dyn FollowerCounts>,
    /// Names the rate-capped containers.
    containers: Arc<dyn ContainerSource>,
}

impl LocalLogs {
    /// How long the page waits for the runtime to name the rate-capped containers.
    const NAMES_TIMEOUT: Duration = Duration::from_secs(2);
    /// The period the "last hour" figures count.
    const HOUR: Duration = Duration::from_secs(3_600);

    /// The figures of `state`, with the recorder's counters behind `sink`, the rate cap's of
    /// `followers`, and container names from `containers`.
    pub fn new(
        state: LogStoreState,
        sink: LogSink,
        followers: Arc<dyn FollowerCounts>,
        containers: Arc<dyn ContainerSource>,
    ) -> Self {
        Self {
            state,
            sink,
            followers,
            containers,
        }
    }

    /// The capped containers by name (their short id when the runtime does not know them),
    /// most dropped lines first.
    async fn named(&self, capped: BTreeMap<ContainerId, u64>) -> Vec<(String, u64)> {
        if capped.is_empty() {
            return Vec::new();
        }
        let names: HashMap<ContainerId, String> =
            match tokio::time::timeout(Self::NAMES_TIMEOUT, self.containers.managed()).await {
                Ok(Ok(containers)) => containers.into_iter().map(|c| (c.id, c.name)).collect(),
                _ => HashMap::new(),
            };
        let mut named: Vec<(String, u64)> = capped
            .into_iter()
            .map(|(id, lines)| {
                let name = names.get(&id).cloned().unwrap_or_else(|| {
                    let id: &str = id.as_ref();
                    id.chars().take(12).collect()
                });
                (name, lines)
            })
            .collect();
        named.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        named
    }
}

#[async_trait::async_trait]
impl LogFacts for LocalLogs {
    async fn read(&self) -> LogHealth {
        let store = match &self.state {
            LogStoreState::Open(store) => store,
            LogStoreState::Unavailable(error) => return LogHealth::Unavailable(error.clone()),
        };
        let mut problem = None;
        let mut note = |err: crate::store::StoreError| {
            problem.get_or_insert_with(|| {
                format!("Cannot read the log store: {:#}", anyhow::Error::from(err))
            });
        };
        let used_bytes = store.used_bytes().await.map_err(&mut note).ok();
        let oldest = store
            .oldest_record_time()
            .await
            .map_err(&mut note)
            .ok()
            .flatten()
            .map(SystemTime::from);
        let since = Timestamp::now() - Self::HOUR;
        let last_hour = store.rows_since(since).await.map_err(&mut note).ok();
        let sweep = *self.sink.sweeps().borrow();
        LogHealth::Open(Box::new(LogFigures {
            path: Some(store.path().to_owned()),
            used_bytes,
            oldest,
            followed: self.followers.followed(),
            last_hour,
            sweep,
            drops: self.sink.drops(),
            capped: self.named(self.followers.capped()).await,
            hoster_capped: HosterRecords::global().capped(),
            problem,
        }))
    }
}
