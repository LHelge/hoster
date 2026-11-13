//! The log recorder (ADR-0067 §6): the one writer of `logs.db`.
//!
//! Producers (the container log followers, the proxy, hoster's own layer of ADR-0068, the OTLP
//! receiver of ADR-0069) hold a [`LogSink`] and offer it records, request rows and spans. The sink never awaits: when the bounded
//! channel to the [`LogRecorder`] is full, or there is no recorder because `logs.db` could not
//! be opened, the item is dropped and counted ([`Drops`]). The recorder commits what it has
//! received in one transaction about once a second, or as soon as a batch reaches its cap, and
//! announces the newest committed ids ([`LogHead`]) for pages and follow streams.
//!
//! The recorder also keeps the store within its retention (ADR-0067 §7): it starts a pass at
//! startup and hourly and advances it one bounded step at a time between its commits, so the
//! one writer stays the one writer and a big sweep never delays the commits for long. What the
//! last pass removed is announced like the head ([`LogSink::sweeps`]).
//!
//! The supervisor stops the producers first and then the recorder, which commits what is left
//! in the channel before it returns (docs/architecture.md §4, shutdown step 6).
//!
//! What the recorder logs itself is under this module's target (`hoster::logs::recorder`),
//! which hoster's own layer excludes (ADR-0068 §2), so a failing commit cannot feed itself.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::store::{
    LogBatch, LogCursor, LogHead, LogRecord, LogStore, RequestRow, Retention, Span, StoreError,
    Stream, Sweep, SweepReport, Timestamp,
};

/// One item on its way to the recorder.
#[derive(Debug)]
enum Entry {
    /// A record, with the cursor its follower reached with it, if it came from a container.
    Record {
        record: LogRecord,
        cursor: Option<LogCursor>,
    },
    Request(RequestRow),
    /// A span a container exported over OTLP.
    Span(Span),
    /// The container no longer exists: delete its cursor.
    ForgetCursor(String),
}

/// What was lost since hoster started, by cause.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drops {
    /// Container records dropped because the recorder's channel was full or there is none.
    pub records: u64,
    /// hoster's own records dropped the same way (ADR-0068 §4).
    pub hoster_records: u64,
    /// Records received over OTLP dropped the same way (ADR-0069 §7).
    pub otlp_records: u64,
    /// Request rows dropped the same way.
    pub requests: u64,
    /// Spans received over OTLP dropped the same way.
    pub spans: u64,
    /// Commits that failed; their batches are lost.
    pub failed_commits: u64,
    /// Records, request rows and spans lost with those batches.
    pub lost_in_failed_commits: u64,
}

impl Drops {
    /// Nothing was lost.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The counters behind [`Drops`], shared by every sink and the recorder.
#[derive(Debug, Default)]
struct Counters {
    records: AtomicU64,
    hoster_records: AtomicU64,
    otlp_records: AtomicU64,
    requests: AtomicU64,
    spans: AtomicU64,
    failed_commits: AtomicU64,
    lost_in_failed_commits: AtomicU64,
}

impl Counters {
    fn count(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    fn snapshot(&self) -> Drops {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Drops {
            records: read(&self.records),
            hoster_records: read(&self.hoster_records),
            otlp_records: read(&self.otlp_records),
            requests: read(&self.requests),
            spans: read(&self.spans),
            failed_commits: read(&self.failed_commits),
            lost_in_failed_commits: read(&self.lost_in_failed_commits),
        }
    }
}

/// A producer's handle to the recorder. Cheap to clone; no method ever awaits.
#[derive(Debug, Clone)]
pub struct LogSink {
    entries: mpsc::Sender<Entry>,
    counters: Arc<Counters>,
    head: watch::Receiver<LogHead>,
    sweeps: watch::Receiver<Option<SweepReport>>,
}

impl LogSink {
    /// A sink with no recorder behind it, for when `logs.db` could not be opened: it drops and
    /// counts everything, so producers need no special case.
    pub fn disconnected() -> Self {
        let (entries, _) = mpsc::channel(1);
        let (_, head) = watch::channel(LogHead::default());
        let (_, sweeps) = watch::channel(None);
        Self {
            entries,
            counters: Arc::default(),
            head,
            sweeps,
        }
    }

    /// Offers a record that came from no container's output: hoster's own (ADR-0068), or one a
    /// container exported over OTLP (ADR-0069 §6). `false` when it was dropped.
    pub fn try_record(&self, record: LogRecord) -> bool {
        self.offer(Entry::Record {
            record,
            cursor: None,
        })
    }

    /// Offers a record a container's follower read, with the cursor it reached with it; both
    /// are committed together. `false` when it was dropped (and the cursor with it).
    pub fn try_record_line(&self, record: LogRecord, cursor: LogCursor) -> bool {
        self.offer(Entry::Record {
            record,
            cursor: Some(cursor),
        })
    }

    /// Offers one of the proxy's request rows. `false` when it was dropped.
    pub fn try_request(&self, request: RequestRow) -> bool {
        self.offer(Entry::Request(request))
    }

    /// Offers a span a container exported over OTLP (ADR-0069 §6). `false` when it was
    /// dropped.
    pub fn try_span(&self, span: Span) -> bool {
        self.offer(Entry::Span(span))
    }

    /// Asks for the cursor of `container`, which no longer exists, to be deleted, after every
    /// line offered before. `false` when the request was dropped, which is not counted: the
    /// followers' hourly sweep asks again.
    pub fn try_forget_cursor(&self, container: &str) -> bool {
        self.offer(Entry::ForgetCursor(container.to_owned()))
    }

    /// What was lost since hoster started.
    pub fn drops(&self) -> Drops {
        self.counters.snapshot()
    }

    /// The newest committed ids, updated after every commit.
    pub fn head(&self) -> watch::Receiver<LogHead> {
        self.head.clone()
    }

    /// What the last finished retention pass removed and how large it left the file; `None`
    /// until the first one since hoster started has finished. Not persisted.
    pub fn sweeps(&self) -> watch::Receiver<Option<SweepReport>> {
        self.sweeps.clone()
    }

    /// A sink whose records a test reads straight off its channel of `capacity`, with no
    /// recorder behind it.
    #[cfg(test)]
    pub fn tapped(capacity: usize) -> (Self, Tap) {
        let (entries, rx) = mpsc::channel(capacity);
        let (_, head) = watch::channel(LogHead::default());
        let (_, sweeps) = watch::channel(None);
        let sink = Self {
            entries,
            counters: Arc::default(),
            head,
            sweeps,
        };
        (sink, Tap(rx))
    }

    fn offer(&self, entry: Entry) -> bool {
        let counter = match &entry {
            Entry::Record { record, .. } => Some(match record.stream {
                Stream::Hoster => &self.counters.hoster_records,
                Stream::Otlp => &self.counters.otlp_records,
                Stream::Stdout | Stream::Stderr => &self.counters.records,
            }),
            Entry::Request(_) => Some(&self.counters.requests),
            Entry::Span(_) => Some(&self.counters.spans),
            Entry::ForgetCursor(_) => None,
        };
        let sent = self.entries.try_send(entry).is_ok();
        if !sent && let Some(counter) = counter {
            Counters::count(counter, 1);
        }
        sent
    }
}

/// The records offered to a [`LogSink::tapped`] sink, for tests.
#[cfg(test)]
#[derive(Debug)]
pub struct Tap(mpsc::Receiver<Entry>);

#[cfg(test)]
impl Tap {
    /// The next record and the cursor it came with; `None` once every sink is gone. Request
    /// rows are skipped.
    pub async fn next(&mut self) -> Option<(LogRecord, Option<LogCursor>)> {
        loop {
            match self.0.recv().await? {
                Entry::Record { record, cursor } => return Some((record, cursor)),
                Entry::Request(_) | Entry::Span(_) | Entry::ForgetCursor(_) => {}
            }
        }
    }

    /// The next request row; `None` once every sink is gone. Records are skipped.
    pub async fn next_request(&mut self) -> Option<RequestRow> {
        loop {
            if let Entry::Request(request) = self.0.recv().await? {
                return Some(request);
            }
        }
    }

    /// The request rows offered so far, without waiting. Records are skipped.
    pub fn drain_requests(&mut self) -> Vec<RequestRow> {
        let mut requests = Vec::new();
        while let Ok(entry) = self.0.try_recv() {
            if let Entry::Request(request) = entry {
                requests.push(request);
            }
        }
        requests
    }

    /// The spans offered so far, without waiting. Records and request rows are skipped.
    pub fn drain_spans(&mut self) -> Vec<Span> {
        let mut spans = Vec::new();
        while let Ok(entry) = self.0.try_recv() {
            if let Entry::Span(span) = entry {
                spans.push(span);
            }
        }
        spans
    }

    /// The records offered so far, without waiting.
    pub fn drain(&mut self) -> Vec<(LogRecord, Option<LogCursor>)> {
        let mut records = Vec::new();
        while let Ok(entry) = self.0.try_recv() {
            if let Entry::Record { record, cursor } = entry {
                records.push((record, cursor));
            }
        }
        records
    }
}

/// The one writer of `logs.db` (ADR-0067 §6).
#[derive(Debug)]
pub struct LogRecorder {
    store: LogStore,
    entries: mpsc::Receiver<Entry>,
    counters: Arc<Counters>,
    head: watch::Sender<LogHead>,
    sweeps: watch::Sender<Option<SweepReport>>,
    /// [`Self::COMMIT_INTERVAL`], shorter in tests.
    interval: Duration,
    /// ADR-0067 §7's limits, smaller in tests.
    retention: Retention,
}

impl LogRecorder {
    /// Items the channel holds before producers' items are dropped: several seconds of a busy
    /// server's output.
    pub const CHANNEL_CAPACITY: usize = 16_384;
    /// How long received items wait before they are committed.
    pub const COMMIT_INTERVAL: Duration = Duration::from_secs(1);
    /// Records plus request rows plus spans that are committed at once, without waiting for the interval,
    /// so a burst never builds one huge transaction.
    pub const BATCH_CAP: usize = 4_096;
    /// How often a retention pass starts after the one at startup.
    pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

    /// A recorder writing to `store`, and the sink producers offer it items through. The head
    /// starts at what `store` already holds.
    pub async fn new(store: LogStore) -> Result<(Self, LogSink), StoreError> {
        let head = store.head().await?;
        let (entries_tx, entries) = mpsc::channel(Self::CHANNEL_CAPACITY);
        let (head_tx, head_rx) = watch::channel(head);
        let (sweeps_tx, sweeps_rx) = watch::channel(None);
        let counters = Arc::<Counters>::default();
        let sink = LogSink {
            entries: entries_tx,
            counters: Arc::clone(&counters),
            head: head_rx,
            sweeps: sweeps_rx,
        };
        let recorder = Self {
            store,
            entries,
            counters,
            head: head_tx,
            sweeps: sweeps_tx,
            interval: Self::COMMIT_INTERVAL,
            retention: Retention::default(),
        };
        Ok((recorder, sink))
    }

    /// The same recorder committing after `interval` instead.
    #[cfg(test)]
    pub(crate) fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// The same recorder sweeping to `retention` instead.
    #[cfg(test)]
    pub(crate) fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    /// Commits what arrives until `stop` fires, then commits what is left in the channel and
    /// returns. Failed commits are logged and counted and never end the loop. Retention passes
    /// run at startup and every [`Self::SWEEP_INTERVAL`], one step per turn of the loop; a
    /// failed one is logged and tried again at the next interval, and one still running at
    /// shutdown is left for the next start.
    pub async fn run(mut self, stop: CancellationToken) {
        let mut batch = Batch::default();
        let mut deadline: Option<Instant> = None;
        let mut sweep: Option<Sweep> = None;
        let mut next_sweep = Instant::now();
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                entry = self.entries.recv() => {
                    let Some(entry) = entry else { break };
                    batch.push(entry);
                    if batch.len() >= Self::BATCH_CAP {
                        self.commit(&mut batch).await;
                        deadline = None;
                    } else if deadline.is_none() {
                        deadline = Some(Instant::now() + self.interval);
                    }
                }
                () = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)),
                    if deadline.is_some() =>
                {
                    self.commit(&mut batch).await;
                    deadline = None;
                }
                () = tokio::time::sleep_until(next_sweep), if sweep.is_none() => {
                    sweep = Some(self.store.sweep(Timestamp::now(), self.retention));
                    next_sweep = Instant::now() + Self::SWEEP_INTERVAL;
                }
                stepped = Self::step(&mut sweep), if sweep.is_some() => {
                    match stepped {
                        Ok(None) => {}
                        Ok(Some(report)) => {
                            sweep = None;
                            Self::log_sweep(&report);
                            self.sweeps.send_replace(Some(report));
                        }
                        Err(err) => {
                            sweep = None;
                            tracing::warn!(
                                error = format!("{:#}", anyhow::Error::from(err)),
                                "cannot sweep the log store to its retention; trying again in an hour"
                            );
                        }
                    }
                }
            }
        }
        // Every producer has stopped (or is dropped from here on): what is queued is all.
        self.entries.close();
        while let Ok(entry) = self.entries.try_recv() {
            batch.push(entry);
            if batch.len() >= Self::BATCH_CAP {
                self.commit(&mut batch).await;
            }
        }
        self.commit(&mut batch).await;
        tracing::debug!("log recorder stopped");
    }

    /// The next step of `sweep`; pending forever when there is none.
    async fn step(sweep: &mut Option<Sweep>) -> Result<Option<SweepReport>, StoreError> {
        match sweep {
            Some(sweep) => sweep.step().await,
            None => std::future::pending().await,
        }
    }

    fn log_sweep(report: &SweepReport) {
        tracing::debug!(
            expired_records = report.expired.records,
            expired_requests = report.expired.requests,
            capped_records = report.capped.records,
            capped_requests = report.capped.requests,
            expired_spans = report.expired.spans,
            capped_spans = report.capped.spans,
            file_bytes = report.file_bytes,
            "swept the log store to its retention"
        );
    }

    /// Writes `batch` and empties it. A failure loses the batch, counted.
    async fn commit(&self, batch: &mut Batch) {
        let batch = std::mem::take(batch).into_log_batch();
        if batch.is_empty() {
            return;
        }
        match self.store.append(&batch).await {
            Ok(head) => {
                self.head.send_replace(head);
            }
            Err(err) => {
                let lost = u64::try_from(batch.len()).unwrap_or(u64::MAX);
                Counters::count(&self.counters.failed_commits, 1);
                Counters::count(&self.counters.lost_in_failed_commits, lost);
                tracing::warn!(
                    error = format!("{:#}", anyhow::Error::from(err)),
                    lost,
                    "cannot write to the log store; dropping the batch"
                );
            }
        }
    }
}

/// What the recorder collected since its last commit.
#[derive(Debug, Default)]
struct Batch {
    records: Vec<LogRecord>,
    requests: Vec<RequestRow>,
    spans: Vec<Span>,
    /// The newest cursor per container; followers send lines in order.
    cursors: BTreeMap<String, LogCursor>,
    /// Containers whose cursors are deleted.
    forgotten: BTreeSet<String>,
}

impl Batch {
    fn push(&mut self, entry: Entry) {
        match entry {
            Entry::Record { record, cursor } => {
                self.records.push(record);
                if let Some(cursor) = cursor {
                    self.forgotten.remove(&cursor.container);
                    self.cursors.insert(cursor.container.clone(), cursor);
                }
            }
            Entry::Request(request) => self.requests.push(request),
            Entry::Span(span) => self.spans.push(span),
            Entry::ForgetCursor(container) => {
                self.cursors.remove(&container);
                self.forgotten.insert(container);
            }
        }
    }

    fn len(&self) -> usize {
        self.records.len() + self.requests.len() + self.spans.len()
    }

    fn into_log_batch(self) -> LogBatch {
        LogBatch {
            records: self.records,
            requests: self.requests,
            spans: self.spans,
            cursors: self.cursors.into_values().collect(),
            forgotten: self.forgotten.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests;
