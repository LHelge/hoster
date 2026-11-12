//! The log recorder (ADR-0067 §6): the one writer of `logs.db`.
//!
//! Producers (the container log followers, the proxy, hoster's own layer of ADR-0068) hold a
//! [`LogSink`] and offer it records and request rows. The sink never awaits: when the bounded
//! channel to the [`LogRecorder`] is full, or there is no recorder because `logs.db` could not
//! be opened, the item is dropped and counted ([`Drops`]). The recorder commits what it has
//! received in one transaction about once a second, or as soon as a batch reaches its cap, and
//! announces the newest committed ids ([`LogHead`]) for pages and follow streams.
//!
//! The supervisor stops the producers first and then the recorder, which commits what is left
//! in the channel before it returns (docs/architecture.md §4, shutdown step 6).
//!
//! What the recorder logs itself is under this module's target (`hoster::logs::recorder`),
//! which hoster's own layer excludes (ADR-0068 §2), so a failing commit cannot feed itself.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::store::{
    LogBatch, LogCursor, LogHead, LogRecord, LogStore, RequestRow, StoreError, Stream,
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
}

/// What was lost since hoster started, by cause.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drops {
    /// Container records dropped because the recorder's channel was full or there is none.
    pub records: u64,
    /// hoster's own records dropped the same way (ADR-0068 §4).
    pub hoster_records: u64,
    /// Request rows dropped the same way.
    pub requests: u64,
    /// Commits that failed; their batches are lost.
    pub failed_commits: u64,
    /// Records and request rows lost with those batches.
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
    requests: AtomicU64,
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
            requests: read(&self.requests),
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
}

impl LogSink {
    /// A sink with no recorder behind it, for when `logs.db` could not be opened: it drops and
    /// counts everything, so producers need no special case.
    pub fn disconnected() -> Self {
        let (entries, _) = mpsc::channel(1);
        let (_, head) = watch::channel(LogHead::default());
        Self {
            entries,
            counters: Arc::default(),
            head,
        }
    }

    /// Offers a record that came from no container (hoster's own, ADR-0068). `false` when it
    /// was dropped.
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

    /// What was lost since hoster started.
    pub fn drops(&self) -> Drops {
        self.counters.snapshot()
    }

    /// The newest committed ids, updated after every commit.
    pub fn head(&self) -> watch::Receiver<LogHead> {
        self.head.clone()
    }

    /// A sink whose records a test reads straight off its channel of `capacity`, with no
    /// recorder behind it.
    #[cfg(test)]
    pub fn tapped(capacity: usize) -> (Self, Tap) {
        let (entries, rx) = mpsc::channel(capacity);
        let (_, head) = watch::channel(LogHead::default());
        let sink = Self {
            entries,
            counters: Arc::default(),
            head,
        };
        (sink, Tap(rx))
    }

    fn offer(&self, entry: Entry) -> bool {
        let counter = match &entry {
            Entry::Record { record, .. } if record.stream == Stream::Hoster => {
                &self.counters.hoster_records
            }
            Entry::Record { .. } => &self.counters.records,
            Entry::Request(_) => &self.counters.requests,
        };
        let sent = self.entries.try_send(entry).is_ok();
        if !sent {
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
                Entry::Request(_) => {}
            }
        }
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
    /// [`Self::COMMIT_INTERVAL`], shorter in tests.
    interval: Duration,
}

impl LogRecorder {
    /// Items the channel holds before producers' items are dropped: several seconds of a busy
    /// server's output.
    pub const CHANNEL_CAPACITY: usize = 16_384;
    /// How long received items wait before they are committed.
    pub const COMMIT_INTERVAL: Duration = Duration::from_secs(1);
    /// Records plus request rows that are committed at once, without waiting for the interval,
    /// so a burst never builds one huge transaction.
    pub const BATCH_CAP: usize = 4_096;

    /// A recorder writing to `store`, and the sink producers offer it items through. The head
    /// starts at what `store` already holds.
    pub async fn new(store: LogStore) -> Result<(Self, LogSink), StoreError> {
        let head = store.head().await?;
        let (entries_tx, entries) = mpsc::channel(Self::CHANNEL_CAPACITY);
        let (head_tx, head_rx) = watch::channel(head);
        let counters = Arc::<Counters>::default();
        let sink = LogSink {
            entries: entries_tx,
            counters: Arc::clone(&counters),
            head: head_rx,
        };
        let recorder = Self {
            store,
            entries,
            counters,
            head: head_tx,
            interval: Self::COMMIT_INTERVAL,
        };
        Ok((recorder, sink))
    }

    /// The same recorder committing after `interval` instead.
    #[cfg(test)]
    pub(crate) fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Commits what arrives until `stop` fires, then commits what is left in the channel and
    /// returns. Failed commits are logged and counted and never end the loop.
    pub async fn run(mut self, stop: CancellationToken) {
        let mut batch = Batch::default();
        let mut deadline: Option<Instant> = None;
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
    /// The newest cursor per container; followers send lines in order.
    cursors: BTreeMap<String, i64>,
}

impl Batch {
    fn push(&mut self, entry: Entry) {
        match entry {
            Entry::Record { record, cursor } => {
                self.records.push(record);
                if let Some(cursor) = cursor {
                    self.cursors.insert(cursor.container, cursor.time_ns);
                }
            }
            Entry::Request(request) => self.requests.push(request),
        }
    }

    fn len(&self) -> usize {
        self.records.len() + self.requests.len()
    }

    fn into_log_batch(self) -> LogBatch {
        LogBatch {
            records: self.records,
            requests: self.requests,
            cursors: self
                .cursors
                .into_iter()
                .map(|(container, time_ns)| LogCursor { container, time_ns })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests;
