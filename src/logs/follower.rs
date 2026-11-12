//! The container log followers (ADR-0067 §1): one task per managed container hoster started,
//! reading its stdout and stderr through the runtime's followed log, from the moment hoster
//! starts it until it is removed, and handing each line to the recorder as a record.
//!
//! [`LogFollowers`] is the registry, keyed by container id and owned by the supervisor through
//! the orchestrator, which calls [`LogFollowers::follow`] after every start and
//! [`LogFollowers::forget`] after every removal. A container that exits ends its follower; the
//! registry keeps where it got to (its cursor), so the follower of its next start, the
//! reconciler's restart (ADR-0028) or a deployment's, reads on from there without repeating a
//! line. A stream that ends while the container still runs (podman restarting under hoster) is
//! opened again from the cursor, with backoff.
//!
//! A record's stack, service and generation come from the container's labels, never from the
//! line; its time is podman's stamp of the line (ADR-0067 §3). The cursor of each line travels
//! with it to the recorder and is committed with it ([`LogSink::try_record_line`]). Lines over
//! the [`RateCap`] are dropped, and one marker record per gap says how many.
//!
//! A cursor names a line by podman's stamp and its place among consecutive lines stamped alike
//! ([`LogCursor::nth`]). Every read starts at the top of podman's log, which the `k8s-file` size
//! cap keeps small (ADR-0044), rather than at a `since`: the follower skips the log up to and
//! including the cursor's line, by position, and takes everything after it whatever its stamp.
//! So lines are neither stored twice nor lost when stamps repeat, and lines written after the
//! clock stepped backwards are still read. When the log's first line is already past the cursor,
//! podman truncated the log over lines hoster had not read, and one marker record says so; when
//! the cursor's line is missing further down, the marker says lines may have been lost.
//!
//! After a hoster restart, the supervisor seeds the registry from the log store's cursors
//! ([`LogFollowers::resume_from`]) before the startup pass follows every managed container,
//! running or not ([`LogFollowers::follow`]): a stopped one's follower reads what is left and
//! ends. Cursors of containers that no longer exist are deleted when a container is forgotten,
//! and by a sweep at startup and hourly ([`LogFollowers::sweep_hourly`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::cap::{Gap, RateCap};
use super::{LogSink, ParsedLine};
use crate::runtime::{
    ContainerId, ContainerRuntime, LogLine, LogOptions, LogStream, ObjectKind, RuntimeError,
    ServiceGeneration,
};
use crate::store::{Level, LogCursor, LogRecord, LogStore, StoreError, Stream, Timestamp};

impl From<LogStream> for Stream {
    fn from(stream: LogStream) -> Self {
        match stream {
            LogStream::Stdout => Stream::Stdout,
            LogStream::Stderr => Stream::Stderr,
        }
    }
}

/// The registry of container log followers. Cheap to clone; every clone is the same registry.
pub struct LogFollowers<R> {
    shared: Arc<Shared<R>>,
}

impl<R> Clone for LogFollowers<R> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

struct Shared<R> {
    runtime: Arc<R>,
    sink: LogSink,
    stop: CancellationToken,
    tasks: TaskTracker,
    containers: Mutex<HashMap<ContainerId, Entry>>,
    /// Notified whenever a follower ends.
    ended: Notify,
}

/// What the registry knows about one container.
#[derive(Debug, Default)]
struct Entry {
    /// A follower task is running for it.
    live: bool,
    /// It was started again while its follower was running: the follower reads on rather than
    /// end, even if it finds the container stopped.
    again: bool,
    /// It was removed while its follower was running: the entry goes when the follower ends.
    removed: bool,
    /// The last line read (or dropped): where its next follower reads on from.
    after: Option<Mark>,
    /// Lines the rate cap dropped since hoster started.
    capped: u64,
}

impl<R> Shared<R> {
    fn lock(&self) -> MutexGuard<'_, HashMap<ContainerId, Entry>> {
        self.containers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Removes the entry of `id`, which no longer exists, and asks for its stored cursor to be
    /// deleted after the lines already offered.
    fn remove(&self, containers: &mut HashMap<ContainerId, Entry>, id: &ContainerId) {
        containers.remove(id);
        self.sink.try_forget_cursor(id.as_ref());
    }
}

/// A line's place in a container's log: podman's stamp of it and its place in the run of
/// consecutive lines with that stamp, 1 for the first. What a [`LogCursor`] stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Mark {
    /// ns since the epoch.
    time_ns: i64,
    nth: i64,
}

impl Mark {
    /// The mark of a line stamped `time_ns` read right after the line marked `previous`.
    fn next(previous: Option<Mark>, time_ns: i64) -> Self {
        match previous {
            Some(previous) if previous.time_ns == time_ns => Self {
                time_ns,
                nth: previous.nth.saturating_add(1),
            },
            _ => Self { time_ns, nth: 1 },
        }
    }
}

impl From<&LogCursor> for Mark {
    fn from(cursor: &LogCursor) -> Self {
        Self {
            time_ns: cursor.time_ns,
            nth: cursor.nth.max(1),
        }
    }
}

impl<R: ContainerRuntime> LogFollowers<R> {
    /// A registry whose followers read through `runtime`, offer records to `sink` and stop when
    /// `stop` fires.
    pub fn new(runtime: Arc<R>, sink: LogSink, stop: CancellationToken) -> Self {
        Self {
            shared: Arc::new(Shared {
                runtime,
                sink,
                stop,
                tasks: TaskTracker::new(),
                containers: Mutex::default(),
                ended: Notify::new(),
            }),
        }
    }

    /// How long [`Self::drained`] waits at most.
    const DRAIN: Duration = Duration::from_secs(2);
    /// How often [`Self::sweep_hourly`] deletes the cursors of containers that are gone.
    const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

    /// Follows container `id`, which hoster has just started (or found after a restart), from
    /// where its last follower got to, or from the start of its log. A container that is not managed (`hoster.*` labels,
    /// [`ServiceGeneration`]) is never followed. Idempotent while a follower runs.
    pub fn follow(&self, id: &ContainerId) {
        if self.shared.stop.is_cancelled() {
            return;
        }
        let after = {
            let mut containers = self.shared.lock();
            let entry = containers.entry(id.clone()).or_default();
            if entry.live {
                entry.again = true;
                return;
            }
            entry.live = true;
            entry.after
        };
        let tail = Tail::new(Arc::clone(&self.shared), id.clone(), after);
        self.shared.tasks.spawn(tail.run());
    }

    /// Forgets container `id`, which hoster has removed, and its stored cursor. A follower
    /// still reading it reads to the end of its stream first.
    pub fn forget(&self, id: &ContainerId) {
        let mut containers = self.shared.lock();
        match containers.get_mut(id) {
            Some(entry) if entry.live => entry.removed = true,
            _ => self.shared.remove(&mut containers, id),
        }
    }

    /// Waits, for [`Self::DRAIN`] at most, until no follower reads container `id`: for a
    /// stopped container about to be removed, so that what it printed last is read first.
    pub async fn drained(&self, id: &ContainerId) {
        let wait = async {
            loop {
                let ended = self.shared.ended.notified();
                tokio::pin!(ended);
                ended.as_mut().enable();
                if !self.shared.lock().get(id).is_some_and(|entry| entry.live) {
                    return;
                }
                ended.await;
            }
        };
        if tokio::time::timeout(Self::DRAIN, wait).await.is_err() {
            tracing::debug!(container = %id, "log follower still reading; removing the container anyway");
        }
    }

    /// Sets where container `id`'s next follower starts reading: after the line `cursor`
    /// names, as the log store's cursor says after a hoster restart. Ignored while a follower
    /// runs or when the registry already knows where the container's log was read to.
    pub fn resume_after(&self, cursor: &LogCursor) {
        let mut containers = self.shared.lock();
        let entry = containers
            .entry(cursor.container.clone().into())
            .or_default();
        if !entry.live && entry.after.is_none() {
            entry.after = Some(Mark::from(cursor));
        }
    }

    /// Seeds the registry with every cursor `store` holds ([`Self::resume_after`]): call it
    /// after a restart, before any container is followed. Returns how many there were.
    pub async fn resume_from(&self, store: &LogStore) -> Result<usize, StoreError> {
        let cursors = store.cursors().await?;
        for cursor in &cursors {
            self.resume_after(cursor);
        }
        Ok(cursors.len())
    }

    /// Deletes the cursors of containers that no longer exist, now and every
    /// [`Self::SWEEP_INTERVAL`] until hoster stops: those removed while hoster was down, and
    /// any whose deletion was dropped on the way to the recorder.
    pub fn sweep_hourly(&self, store: LogStore) {
        let followers = self.clone();
        self.shared.tasks.spawn(async move {
            let stop = followers.shared.stop.clone();
            let mut ticks = tokio::time::interval(Self::SWEEP_INTERVAL);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = stop.cancelled() => return,
                    _ = ticks.tick() => {}
                }
                followers.sweep(&store).await;
            }
        });
    }

    /// One sweep of [`Self::sweep_hourly`]; returns how many cursors it asked to delete.
    async fn sweep(&self, store: &LogStore) -> usize {
        // What may be stale is read before the listing: a container that existed then and is
        // not listed is gone for good, as ids are never reused.
        let mut candidates: BTreeSet<ContainerId> = self.shared.lock().keys().cloned().collect();
        match store.cursors().await {
            Ok(cursors) => candidates.extend(cursors.into_iter().map(|c| c.container.into())),
            Err(err) => {
                tracing::debug!(%err, "cannot read the log cursors to sweep them");
                return 0;
            }
        }
        let present: BTreeSet<ContainerId> = match self.shared.runtime.list_managed().await {
            Ok(containers) => containers.into_iter().map(|c| c.id).collect(),
            Err(err) => {
                tracing::debug!(%err, "cannot list containers to sweep the log cursors");
                return 0;
            }
        };
        let mut containers = self.shared.lock();
        let mut swept = 0;
        for id in candidates.difference(&present) {
            if containers.get(id).is_none_or(|entry| !entry.live) {
                self.shared.remove(&mut containers, id);
                swept += 1;
            }
        }
        if swept > 0 {
            tracing::debug!(swept, "deleted the log cursors of containers that are gone");
        }
        swept
    }

    /// The containers being followed right now.
    #[allow(dead_code, reason = "shown on the System page (task uw9)")]
    pub fn followed(&self) -> Vec<ContainerId> {
        let mut followed: Vec<_> = self
            .shared
            .lock()
            .iter()
            .filter(|(_, entry)| entry.live)
            .map(|(id, _)| id.clone())
            .collect();
        followed.sort();
        followed
    }

    /// Lines the rate cap dropped since hoster started, per container not yet removed; only
    /// containers that lost any.
    #[allow(dead_code, reason = "shown on the System page (task uw9)")]
    pub fn capped(&self) -> BTreeMap<ContainerId, u64> {
        self.shared
            .lock()
            .iter()
            .filter(|(_, entry)| entry.capped > 0)
            .map(|(id, entry)| (id.clone(), entry.capped))
            .collect()
    }

    /// Stops every follower and waits until they have handed their last records to the sink.
    /// Call it before the recorder stops (docs/architecture.md §4, shutdown step 6).
    pub async fn stop(&self) {
        self.shared.stop.cancel();
        self.shared.tasks.close();
        self.shared.tasks.wait().await;
    }
}

/// How long a follower waits before it opens a stream again, or asks about its container again
/// after the runtime failed to say.
#[derive(Debug)]
struct Backoff {
    next: Duration,
}

impl Backoff {
    const MIN: Duration = Duration::from_millis(250);
    const MAX: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self { next: Self::MIN }
    }

    fn reset(&mut self) {
        self.next = Self::MIN;
    }

    /// The wait before the next attempt; each one doubles the next, up to [`Self::MAX`].
    fn step(&mut self) -> Duration {
        let wait = self.next;
        self.next = (self.next * 2).min(Self::MAX);
        wait
    }
}

/// How a follower's stream ended.
enum Ended {
    /// It ran out or failed; `read` says whether it yielded any line.
    Stream { read: bool },
    /// hoster is stopping.
    Stopped,
}

/// What the runtime says about the container after its stream ended.
enum Next {
    /// Open the stream again, now.
    Reconnect,
    /// Open it again after a wait: it ended while the container runs.
    Retry,
    /// The container stopped or is gone: the follower ends.
    End,
}

/// One container's follower task.
struct Tail<R> {
    shared: Arc<Shared<R>>,
    id: ContainerId,
    /// The last line read (or dropped).
    after: Option<Mark>,
    /// The mark of the last stamped line of the current stream.
    run: Option<Mark>,
    /// While the current stream has not reached the line [`Self::after`] named when it was
    /// opened: that line and those before it were read before.
    seek: Option<Mark>,
    /// The current stream skipped lines while it sought.
    skipped: bool,
    cap: RateCap,
    /// The stream of the first line of the open gap, for its marker.
    gap_stream: Option<Stream>,
}

impl<R: ContainerRuntime> Tail<R> {
    fn new(shared: Arc<Shared<R>>, id: ContainerId, after: Option<Mark>) -> Self {
        Self {
            shared,
            id,
            after,
            run: None,
            seek: None,
            skipped: false,
            cap: RateCap::default(),
            gap_stream: None,
        }
    }

    async fn run(mut self) {
        let Some(source) = self.identify().await else {
            let mut containers = self.shared.lock();
            self.shared.remove(&mut containers, &self.id);
            drop(containers);
            self.shared.ended.notify_waiters();
            return;
        };
        tracing::debug!(container = %self.id, %source, after = ?self.after, "following log");
        let mut backoff = Backoff::new();
        loop {
            let ended = self.read(&source).await;
            self.end_gap(&source);
            let Ended::Stream { read } = ended else {
                break;
            };
            if read {
                backoff.reset();
            }
            match self.next(&mut backoff).await {
                Some(Next::Reconnect) => {}
                Some(Next::Retry) => {
                    if !self.pause(backoff.step()).await {
                        break;
                    }
                }
                Some(Next::End) => return,
                None => break,
            }
        }
        // Stopping: keep where it got to, as an ended follower does.
        self.finish(false);
    }

    /// Which generation of which service the container is, from its labels; `None` when it
    /// is gone, not hoster's, or hoster is stopping.
    async fn identify(&self) -> Option<ServiceGeneration> {
        let mut backoff = Backoff::new();
        loop {
            match self.shared.runtime.inspect(&self.id).await {
                Ok(info) => {
                    return ServiceGeneration::try_from(&info)
                        .inspect_err(|why| {
                            tracing::debug!(container = %self.id, why, "not following an unmanaged container");
                        })
                        .ok();
                }
                Err(err) if Self::is_gone(&err) => return None,
                Err(err) => {
                    tracing::debug!(container = %self.id, %err, "cannot inspect container to follow its log");
                    if !self.pause(backoff.step()).await {
                        return None;
                    }
                }
            }
        }
    }

    /// Reads one followed stream, from the top of the log past the cursor, until it ends or
    /// hoster stops.
    async fn read(&mut self, source: &ServiceGeneration) -> Ended {
        self.seek = self.after;
        self.run = None;
        self.skipped = false;
        let options = LogOptions {
            follow: true,
            tail: None,
            since: None,
        };
        let mut lines = self.shared.runtime.logs(&self.id, options);
        let mut read = false;
        loop {
            tokio::select! {
                biased;
                () = self.shared.stop.cancelled() => return Ended::Stopped,
                line = lines.next() => match line {
                    Some(Ok(line)) => {
                        read = true;
                        self.take(source, line);
                    }
                    Some(Err(err)) => {
                        tracing::debug!(container = %self.id, %err, "container log stream failed");
                        return Ended::Stream { read };
                    }
                    None => return Ended::Stream { read },
                },
            }
        }
    }

    /// Hands one line to the recorder, unless it was read before or the cap drops it.
    fn take(&mut self, source: &ServiceGeneration, line: LogLine) {
        let mark = line
            .time
            .map(|time| Mark::next(self.run, Self::nanos(time)));
        if mark.is_some() {
            self.run = mark;
        }
        let time = line.time.unwrap_or_else(SystemTime::now);
        let stream = Stream::from(line.stream);
        if let (Some(seek), Some(mark)) = (self.seek, mark) {
            if mark < seek {
                self.skipped = true;
                return;
            }
            self.seek = None;
            if mark == seek {
                return;
            }
            // Past the cursor without meeting its line: podman's log no longer holds it.
            let lost = Lost {
                after: Self::time(seek.time_ns),
                before: time,
                certain: !self.skipped,
            };
            self.lost(source, stream, &lost);
        }
        let admission = self.cap.admit(time);
        if let Some(gap) = admission.ended {
            self.mark(source, gap);
        }
        if mark.is_some() {
            self.after = mark;
        }
        if !admission.keep {
            self.gap_stream.get_or_insert(stream);
            return;
        }
        let parsed = ParsedLine::from(line.text.as_str());
        let record = LogRecord {
            time: Timestamp::from(time),
            stack: Some(source.stack.to_string()),
            service: source.service.to_string(),
            generation: Some(source.generation),
            stream,
            level: parsed.level,
            message: parsed.message,
            fields: parsed.fields,
            cut: parsed.cut,
            marker: false,
        };
        self.send(record, mark);
    }

    /// Writes the marker of lines podman no longer held when the stream was opened.
    fn lost(&self, source: &ServiceGeneration, stream: Stream, lost: &Lost) {
        tracing::debug!(container = %self.id, message = lost.message(), "container log lines lost");
        let record = LogRecord {
            time: Timestamp::from(lost.before),
            stack: Some(source.stack.to_string()),
            service: source.service.to_string(),
            generation: Some(source.generation),
            stream,
            level: Some(Level::Warn),
            message: lost.message(),
            fields: None,
            cut: false,
            marker: true,
        };
        self.send(record, None);
    }

    /// Writes the marker of the gap that ended at the end of the stream, if any.
    fn end_gap(&mut self, source: &ServiceGeneration) {
        if let Some(gap) = self.cap.take_gap() {
            self.mark(source, gap);
        }
    }

    /// Writes the marker of `gap`, stamped with its last dropped line and with the cursor of
    /// the last line read, which is that line.
    fn mark(&mut self, source: &ServiceGeneration, gap: Gap) {
        if let Some(entry) = self.shared.lock().get_mut(&self.id) {
            entry.capped += gap.dropped;
        }
        let record = LogRecord {
            time: Timestamp::from(gap.last),
            stack: Some(source.stack.to_string()),
            service: source.service.to_string(),
            generation: Some(source.generation),
            stream: self.gap_stream.take().unwrap_or(Stream::Stdout),
            level: Some(Level::Warn),
            message: gap.message(),
            fields: None,
            cut: false,
            marker: true,
        };
        self.send(record, self.after);
    }

    fn send(&self, record: LogRecord, mark: Option<Mark>) {
        match mark {
            Some(mark) => {
                let cursor = LogCursor {
                    container: self.id.to_string(),
                    time_ns: mark.time_ns,
                    nth: mark.nth,
                };
                self.shared.sink.try_record_line(record, cursor)
            }
            None => self.shared.sink.try_record(record),
        };
    }

    /// What to do once a stream ended; `None` when hoster is stopping.
    async fn next(&mut self, backoff: &mut Backoff) -> Option<Next> {
        loop {
            if self.shared.stop.is_cancelled() {
                return None;
            }
            match self.shared.runtime.inspect(&self.id).await {
                Ok(info) if info.state.is_running() => return Some(Next::Retry),
                Ok(_) => {
                    return Some(if self.finish(false) {
                        Next::End
                    } else {
                        Next::Reconnect
                    });
                }
                Err(err) if Self::is_gone(&err) => {
                    self.finish(true);
                    return Some(Next::End);
                }
                Err(err) => {
                    tracing::debug!(container = %self.id, %err, "cannot inspect followed container");
                    if !self.pause(backoff.step()).await {
                        return None;
                    }
                }
            }
        }
    }

    /// Ends the follower in the registry, keeping its cursor unless the container is `gone`.
    /// `false` when the container was started again meanwhile and the follower reads on.
    fn finish(&self, gone: bool) -> bool {
        let mut containers = self.shared.lock();
        let Some(entry) = containers.get_mut(&self.id) else {
            return true;
        };
        if gone || entry.removed {
            self.shared.remove(&mut containers, &self.id);
            drop(containers);
            self.shared.ended.notify_waiters();
            return true;
        }
        // It started from the entry's point and only reads on from there.
        entry.after = self.after;
        if entry.again && !self.shared.stop.is_cancelled() {
            entry.again = false;
            return false;
        }
        entry.live = false;
        entry.again = false;
        drop(containers);
        self.shared.ended.notify_waiters();
        true
    }

    /// Waits `wait`; `false` when hoster stops meanwhile.
    async fn pause(&self, wait: Duration) -> bool {
        tokio::select! {
            () = self.shared.stop.cancelled() => false,
            () = tokio::time::sleep(wait) => true,
        }
    }

    fn is_gone(err: &RuntimeError) -> bool {
        matches!(
            err,
            RuntimeError::NotFound {
                kind: ObjectKind::Container,
                ..
            }
        )
    }

    fn nanos(time: SystemTime) -> i64 {
        match time.duration_since(UNIX_EPOCH) {
            Ok(since) => i64::try_from(since.as_nanos()).unwrap_or(i64::MAX),
            Err(_) => 0,
        }
    }

    fn time(nanos: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_nanos(u64::try_from(nanos).unwrap_or(0))
    }
}

/// Lines podman's log no longer held when a follower read on from its cursor: hoster was down,
/// or the stream was away, while the size cap truncated the log (ADR-0044).
#[derive(Debug)]
struct Lost {
    /// The time of the cursor's line.
    after: SystemTime,
    /// The time of the first line read past it.
    before: SystemTime,
    /// The log started past the cursor, so lines were certainly lost; otherwise its line was
    /// merely missing further down and they may have been.
    certain: bool,
}

impl Lost {
    /// The marker record's message.
    fn message(&self) -> String {
        let (after, before) = (
            humantime::format_rfc3339_millis(self.after),
            humantime::format_rfc3339_millis(self.before),
        );
        if self.certain {
            format!(
                "lines written between {after} and {before} were lost: podman's log no longer held them when hoster read it"
            )
        } else {
            format!(
                "lines written between {after} and {before} may have been lost: podman's log no longer held the last line hoster had read"
            )
        }
    }
}

#[cfg(test)]
mod tests;
