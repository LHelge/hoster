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
//! Lines are told apart from those already read by their stamp alone: a line stamped with the
//! very nanosecond of the cursor is taken as read.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::cap::{Gap, RateCap};
use super::{LogSink, ParsedLine};
use crate::runtime::{
    ContainerId, ContainerRuntime, LogLine, LogOptions, LogStream, ObjectKind, RuntimeError,
    ServiceGeneration,
};
use crate::store::{Level, LogCursor, LogRecord, Stream, Timestamp};

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
    /// The stamp of the last line read (or dropped), in ns since the epoch.
    after: Option<i64>,
    /// Lines the rate cap dropped since hoster started.
    capped: u64,
}

impl<R> Shared<R> {
    fn lock(&self) -> MutexGuard<'_, HashMap<ContainerId, Entry>> {
        self.containers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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
            }),
        }
    }

    /// Follows container `id`, which hoster has just started, from where its last follower got
    /// to, or from the start of its log. A container that is not managed (`hoster.*` labels,
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

    /// Forgets container `id`, which hoster has removed. A follower still reading it reads to
    /// the end of its stream first.
    pub fn forget(&self, id: &ContainerId) {
        let mut containers = self.shared.lock();
        if let Some(entry) = containers.get_mut(id) {
            if entry.live {
                entry.removed = true;
            } else {
                containers.remove(id);
            }
        }
    }

    /// Sets where container `id`'s next follower starts reading: after the line stamped
    /// `cursor.time_ns`, as the log store's cursor says after a hoster restart. Ignored while a
    /// follower runs or when the registry already knows a later point.
    #[allow(
        dead_code,
        reason = "seeded from the log store's cursors after a restart (task tuh)"
    )]
    pub fn resume_after(&self, cursor: LogCursor) {
        let mut containers = self.shared.lock();
        let entry = containers.entry(cursor.container.into()).or_default();
        if !entry.live {
            entry.after = entry.after.max(Some(cursor.time_ns));
        }
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
    /// The stamp of the last line read, in ns since the epoch.
    after: Option<i64>,
    /// [`Self::after`] when the current stream was opened: its lines up to it were read before.
    resume: Option<i64>,
    cap: RateCap,
    /// The stream of the first line of the open gap, for its marker.
    gap_stream: Option<Stream>,
}

impl<R: ContainerRuntime> Tail<R> {
    fn new(shared: Arc<Shared<R>>, id: ContainerId, after: Option<i64>) -> Self {
        Self {
            shared,
            id,
            after,
            resume: after,
            cap: RateCap::default(),
            gap_stream: None,
        }
    }

    async fn run(mut self) {
        let Some(source) = self.identify().await else {
            self.shared.lock().remove(&self.id);
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

    /// Reads one followed stream from the cursor until it ends or hoster stops.
    async fn read(&mut self, source: &ServiceGeneration) -> Ended {
        self.resume = self.after;
        let options = LogOptions {
            follow: true,
            tail: None,
            since: self.after.map(Self::time),
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
        let stamp = line.time.map(Self::nanos);
        if let (Some(resume), Some(stamp)) = (self.resume, stamp)
            && stamp <= resume
        {
            return;
        }
        let time = line.time.unwrap_or_else(SystemTime::now);
        let stream = Stream::from(line.stream);
        let admission = self.cap.admit(time);
        if let Some(gap) = admission.ended {
            self.mark(source, gap);
        }
        self.after = self.after.max(stamp);
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
        self.send(record, stamp);
    }

    /// Writes the marker of the gap that ended at the end of the stream, if any.
    fn end_gap(&mut self, source: &ServiceGeneration) {
        if let Some(gap) = self.cap.take_gap() {
            self.mark(source, gap);
        }
    }

    /// Writes the marker of `gap`, stamped and with the cursor of its last dropped line.
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
        self.send(record, Some(Self::nanos(gap.last)));
    }

    fn send(&self, record: LogRecord, stamp: Option<i64>) {
        match stamp {
            Some(time_ns) => {
                let cursor = LogCursor {
                    container: self.id.to_string(),
                    time_ns,
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
            containers.remove(&self.id);
            return true;
        }
        entry.after = entry.after.max(self.after);
        if entry.again && !self.shared.stop.is_cancelled() {
            entry.again = false;
            return false;
        }
        entry.live = false;
        entry.again = false;
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

#[cfg(test)]
mod tests;
