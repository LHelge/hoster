//! The followers against [`FakeRuntime`]: lines become records with the container's labels,
//! restarts read on from the cursor, removals are forgotten, the rate cap marks its gaps.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tempfile::TempDir;

use super::*;
use crate::logs::{LogRecorder, Tap};
use crate::runtime::{ContainerSpec, ContainerState, FakeRuntime, Labels, NetworkSpec};
use crate::stack::{Name, PinnedImage};
use crate::store::{DeploymentId, LogStore};

const WAIT: Duration = Duration::from_secs(5);

fn name(s: &str) -> Name {
    s.parse().unwrap()
}

fn generation(service: &str, generation: u32) -> ServiceGeneration {
    ServiceGeneration {
        stack: name("notes"),
        service: name(service),
        generation,
    }
}

/// `ms` milliseconds into a fixed second.
fn at(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(1_700_000_000_000 + ms)
}

fn nanos(time: SystemTime) -> i64 {
    i64::try_from(time.duration_since(UNIX_EPOCH).unwrap().as_nanos()).unwrap()
}

fn line(stream: LogStream, time: SystemTime, text: &str) -> LogLine {
    LogLine {
        stream,
        time: Some(time),
        text: text.to_owned(),
    }
}

fn out(ms: u64, text: &str) -> LogLine {
    line(LogStream::Stdout, at(ms), text)
}

struct Fixture {
    runtime: Arc<FakeRuntime>,
    followers: LogFollowers<FakeRuntime>,
    tap: Tap,
}

impl Fixture {
    fn new() -> Self {
        let runtime = Arc::new(FakeRuntime::new());
        let (sink, tap) = LogSink::tapped(4_096);
        let followers = LogFollowers::new(Arc::clone(&runtime), sink, CancellationToken::new());
        Self {
            runtime,
            followers,
            tap,
        }
    }

    /// Creates and starts the container of `key`, as a deployment would.
    async fn start(&self, key: &ServiceGeneration) -> ContainerId {
        let spec = self.spec(key);
        self.start_spec(&spec).await
    }

    fn spec(&self, key: &ServiceGeneration) -> ContainerSpec {
        let image: PinnedImage = format!("ghcr.io/acme/notes@sha256:{}", "ab".repeat(32))
            .parse()
            .unwrap();
        ContainerSpec::new(key, DeploymentId::from(1), image)
    }

    async fn start_spec(&self, spec: &ContainerSpec) -> ContainerId {
        self.runtime.pull_image(&spec.image).await.unwrap();
        self.runtime
            .ensure_network(&NetworkSpec::for_stack(&name("notes")))
            .await
            .unwrap();
        let id = self.runtime.create_container(spec).await.unwrap();
        self.runtime.start(&id).await.unwrap();
        id
    }

    async fn next(&mut self) -> (LogRecord, Option<LogCursor>) {
        tokio::time::timeout(WAIT, self.tap.next())
            .await
            .expect("a record arrives")
            .expect("the sink is open")
    }

    /// The messages of the next `n` records.
    async fn messages(&mut self, n: usize) -> Vec<String> {
        let mut messages = Vec::new();
        for _ in 0..n {
            messages.push(self.next().await.0.message);
        }
        messages
    }

    /// Waits until no follower runs for `id`.
    async fn ended(&self, id: &ContainerId) {
        tokio::time::timeout(WAIT, async {
            while self.followers.followed().contains(id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the follower ends");
    }

    fn known(&self, id: &ContainerId) -> bool {
        self.followers.shared.lock().contains_key(id)
    }

    /// Lets the followers run, then returns what they offered meanwhile.
    async fn settle(&mut self) -> Vec<String> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.tap
            .drain()
            .into_iter()
            .map(|(r, _)| r.message)
            .collect()
    }
}

#[tokio::test]
async fn records_carry_the_labels_and_the_line_with_its_cursor() {
    let mut f = Fixture::new();
    let old = f.start(&generation("api", 1)).await;
    let new = f.start(&generation("api", 2)).await;
    // Printed before the follower starts, as start-up output is.
    f.runtime.push_log(
        &old,
        out(
            1,
            r#"{"level":"WARN","fields":{"message":"slow start","ms":12}}"#,
        ),
    );
    f.followers.follow(&old);
    let (record, cursor) = f.next().await;
    assert_eq!(
        record,
        LogRecord {
            time: Timestamp::from(at(1)),
            stack: Some("notes".into()),
            service: "api".into(),
            generation: Some(1),
            stream: Stream::Stdout,
            level: Some(Level::Warn),
            message: "slow start".into(),
            fields: Some(r#"{"fields":{"ms":12}}"#.into()),
            cut: false,
            marker: false,
            trace_id: None,
            span_id: None,
            received: None,
        }
    );
    assert_eq!(
        cursor,
        Some(LogCursor {
            container: old.to_string(),
            time_ns: nanos(at(1)),
            nth: 1,
        })
    );

    // Two generations at once, as in a start-first deploy: told apart by generation.
    f.followers.follow(&new);
    f.runtime.push_log(
        &new,
        line(LogStream::Stderr, at(2), "panicked at 'level=error'"),
    );
    let (record, _) = f.next().await;
    assert_eq!(record.generation, Some(2));
    assert_eq!(record.stream, Stream::Stderr);
    assert_eq!(record.level, None, "levels are never guessed in text");
    assert_eq!(record.message, "panicked at 'level=error'");
    assert_eq!(f.followers.followed().len(), 2);
}

#[tokio::test]
async fn a_restarted_container_reads_on_from_where_it_stopped() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    f.runtime.push_log(&id, out(1, "first run"));
    f.followers.follow(&id);
    assert_eq!(f.messages(1).await, ["first run"]);

    f.runtime.set_state(&id, ContainerState::Exited(Some(1)));
    f.ended(&id).await;
    assert!(f.known(&id), "an exited container's cursor is kept");

    // The reconciler restarts it: a new follower, which does not read the first run again.
    f.runtime.start(&id).await.unwrap();
    f.followers.follow(&id);
    f.runtime.push_log(&id, out(2, "second run"));
    assert_eq!(f.messages(1).await, ["second run"]);
    assert_eq!(f.settle().await, Vec::<String>::new());
}

#[tokio::test]
async fn a_crash_looping_container_is_read_to_the_end_of_each_run() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    for run in 1..=3 {
        // It printed and exited before its follower got to it.
        f.runtime.push_log(&id, out(run, &format!("run {run}")));
        f.runtime.set_state(&id, ContainerState::Exited(Some(1)));
        f.followers.follow(&id);
        assert_eq!(f.messages(1).await, [format!("run {run}")]);
        f.ended(&id).await;
        f.runtime.start(&id).await.unwrap();
    }
    assert_eq!(f.settle().await, Vec::<String>::new());
}

#[tokio::test]
async fn following_again_while_the_follower_ends_reads_on_once() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    f.followers.follow(&id);
    f.runtime.push_log(&id, out(1, "one"));
    assert_eq!(f.messages(1).await, ["one"]);
    // Stopped and started again before the follower noticed.
    f.runtime.set_state(&id, ContainerState::Exited(Some(0)));
    f.runtime.push_log(&id, out(2, "two"));
    f.followers.follow(&id);
    assert_eq!(f.messages(1).await, ["two"]);
    f.ended(&id).await;
    assert_eq!(f.settle().await, Vec::<String>::new());
}

#[tokio::test]
async fn a_stream_that_ends_while_the_container_runs_reconnects_from_its_cursor() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    f.followers.follow(&id);
    f.runtime.push_log(&id, out(1, "before"));
    assert_eq!(f.messages(1).await, ["before"]);
    // podman restarting under hoster: the stream ends, the container keeps running.
    f.runtime.set_state(&id, ContainerState::Exited(None));
    f.runtime.set_state(&id, ContainerState::Running);
    f.runtime.push_log(&id, out(2, "while away"));
    assert_eq!(f.messages(1).await, ["while away"]);
    f.runtime.push_log(&id, out(3, "after"));
    assert_eq!(f.messages(1).await, ["after"]);
    assert_eq!(f.followers.followed(), [id]);
}

#[tokio::test]
async fn a_removed_container_is_forgotten() {
    let f = Fixture::new();
    let running = f.start(&generation("api", 1)).await;
    let exited = f.start(&generation("web", 1)).await;
    f.followers.follow(&running);
    f.followers.follow(&exited);
    f.runtime
        .set_state(&exited, ContainerState::Exited(Some(0)));
    f.ended(&exited).await;

    f.runtime.remove(&exited).await.unwrap();
    f.followers.forget(&exited);
    assert!(!f.known(&exited));

    // Removed while its follower reads: the follower ends and takes the entry with it.
    f.runtime.push_log(&running, out(1, "last words"));
    f.runtime.remove(&running).await.unwrap();
    f.followers.forget(&running);
    f.ended(&running).await;
    assert!(!f.known(&running));
    assert_eq!(f.followers.followed(), Vec::<ContainerId>::new());
}

#[tokio::test]
async fn an_unmanaged_container_is_never_followed() {
    let mut f = Fixture::new();
    let mut spec = f.spec(&generation("api", 1));
    spec.labels = Labels::new();
    let id = f.start_spec(&spec).await;
    f.runtime.push_log(&id, out(1, "not hoster's"));
    f.followers.follow(&id);
    f.ended(&id).await;
    assert!(!f.known(&id));
    assert_eq!(f.runtime.log_followers(&id), 0);
    assert_eq!(f.settle().await, Vec::<String>::new());
}

#[tokio::test]
async fn lines_over_the_rate_cap_are_dropped_and_each_gap_marked() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    let cap = u64::from(RateCap::LINES_PER_SECOND);
    for i in 0..cap + 5 {
        f.runtime
            .push_log(&id, out(i % 1_000, &format!("line {i}")));
    }
    f.runtime.push_log(&id, out(1_500, "next second"));
    f.followers.follow(&id);
    let kept = f.messages(usize::try_from(cap).unwrap()).await;
    assert_eq!(kept.last().unwrap(), &format!("line {}", cap - 1));
    let (marker, cursor) = f.next().await;
    assert!(marker.marker);
    assert_eq!(
        marker.message,
        "5 lines dropped: more than 1000 lines a second"
    );
    assert_eq!(marker.service, "api");
    assert_eq!(marker.level, Some(Level::Warn));
    // Stamped with the last dropped line, so a resumed follower does not read the gap again.
    let last_dropped = at((cap + 4) % 1_000);
    assert_eq!(marker.time, Timestamp::from(last_dropped));
    assert_eq!(cursor.unwrap().time_ns, nanos(last_dropped));
    assert_eq!(f.messages(1).await, ["next second"]);
    assert_eq!(f.followers.capped(), BTreeMap::from([(id, 5)]));
}

#[tokio::test]
async fn stopping_ends_every_follower_and_marks_an_open_gap() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    for i in 0..=u64::from(RateCap::LINES_PER_SECOND) {
        f.runtime.push_log(&id, out(0, &format!("line {i}")));
    }
    f.followers.follow(&id);
    f.messages(usize::try_from(RateCap::LINES_PER_SECOND).unwrap())
        .await;
    tokio::time::timeout(WAIT, f.followers.stop())
        .await
        .expect("the followers stop");
    assert_eq!(f.runtime.log_followers(&id), 0);
    let records = f.tap.drain();
    assert_eq!(records.len(), 1);
    assert!(records[0].0.marker, "{records:?}");
    // Nothing is followed once hoster stops.
    f.followers.follow(&id);
    assert_eq!(f.followers.followed(), Vec::<ContainerId>::new());
}

#[tokio::test]
async fn the_cursor_commits_with_the_lines_and_seeds_a_later_follower() {
    let dir = TempDir::new().unwrap();
    let store = LogStore::open(dir.path()).await.unwrap();
    let (recorder, sink) = LogRecorder::new(store.clone()).await.unwrap();
    let stop = CancellationToken::new();
    let recording = tokio::spawn(
        recorder
            .with_interval(Duration::from_millis(10))
            .run(stop.clone()),
    );
    let runtime = Arc::new(FakeRuntime::new());
    let f = Fixture {
        runtime: Arc::clone(&runtime),
        followers: LogFollowers::new(Arc::clone(&runtime), sink, CancellationToken::new()),
        tap: LogSink::tapped(1).1,
    };
    let id = f.start(&generation("api", 1)).await;
    f.runtime.push_log(&id, out(1, "one"));
    f.runtime.push_log(&id, out(2, "two"));
    f.followers.follow(&id);
    tokio::time::timeout(WAIT, async {
        while store.cursor(id.as_ref()).await.unwrap().map(|c| c.time_ns) != Some(nanos(at(2))) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the cursor is committed");
    assert_eq!(store.head().await.unwrap().record, 2);
    f.followers.stop().await;
    stop.cancel();
    recording.await.unwrap();

    // After a restart the cursor seeds the next follower, which reads only what is new.
    let (sink, mut tap) = LogSink::tapped(16);
    let followers = LogFollowers::new(Arc::clone(&runtime), sink, CancellationToken::new());
    assert_eq!(followers.resume_from(&store).await.unwrap(), 1);
    runtime.push_log(&id, out(3, "three"));
    followers.follow(&id);
    let (record, _) = tokio::time::timeout(WAIT, tap.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.message, "three");
}

/// A cursor of `id` at the `nth` line stamped `at(ms)`.
fn cursor(id: &ContainerId, ms: u64, nth: i64) -> LogCursor {
    LogCursor {
        container: id.to_string(),
        time_ns: nanos(at(ms)),
        nth,
    }
}

#[tokio::test]
async fn a_resumed_follower_reads_only_what_came_after_its_cursor() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    for (ms, text) in [(1, "stored"), (2, "stored last"), (3, "while down")] {
        f.runtime.push_log(&id, out(ms, text));
    }
    f.followers.resume_after(&cursor(&id, 2, 1));
    f.followers.follow(&id);
    let (record, cursor) = f.next().await;
    assert_eq!(record.message, "while down");
    assert!(!record.marker);
    assert_eq!(cursor.unwrap().time_ns, nanos(at(3)));
    f.runtime.push_log(&id, out(4, "after"));
    assert_eq!(f.messages(1).await, ["after"]);
    assert_eq!(f.settle().await, Vec::<String>::new());
}

#[tokio::test]
async fn lines_stamped_alike_are_neither_repeated_nor_lost() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    for text in ["a", "b"] {
        f.runtime.push_log(&id, out(5, text));
    }
    f.followers.follow(&id);
    let cursors: Vec<_> = [f.next().await, f.next().await]
        .into_iter()
        .map(|(_, cursor)| cursor.unwrap())
        .collect();
    assert_eq!(cursors, [cursor(&id, 5, 1), cursor(&id, 5, 2)]);
    f.followers.stop().await;

    // hoster restarts after "b"; "c" was printed in the same nanosecond while it was down.
    f.runtime.push_log(&id, out(5, "c"));
    let (sink, mut tap) = LogSink::tapped(16);
    let followers = LogFollowers::new(Arc::clone(&f.runtime), sink, CancellationToken::new());
    followers.resume_after(&cursors[1]);
    followers.follow(&id);
    let (record, stored) = tokio::time::timeout(WAIT, tap.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.message, "c");
    assert_eq!(stored, Some(cursor(&id, 5, 3)));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(tap.drain().is_empty());
}

#[tokio::test]
async fn a_log_truncated_past_the_cursor_is_marked_as_lost() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    f.runtime.push_log(&id, out(1, "stored"));
    // Down for long enough that podman's size cap truncated the log.
    f.runtime.truncate_log(&id);
    f.runtime.push_log(&id, out(900, "kept by podman"));
    f.followers.resume_after(&cursor(&id, 1, 1));
    f.followers.follow(&id);
    let (marker, cursor) = f.next().await;
    assert!(marker.marker);
    assert_eq!(marker.level, Some(Level::Warn));
    assert_eq!(marker.service, "api");
    assert_eq!(marker.time, Timestamp::from(at(900)));
    assert_eq!(
        marker.message,
        "lines written between 2023-11-14T22:13:20.001Z and 2023-11-14T22:13:20.900Z were lost: \
         podman's log no longer held them when hoster read it"
    );
    assert_eq!(cursor, None);
    assert_eq!(f.messages(1).await, ["kept by podman"]);
    assert_eq!(f.settle().await, Vec::<String>::new());
}

#[tokio::test]
async fn a_cursor_line_missing_further_down_marks_lines_that_may_be_lost() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    f.runtime.push_log(&id, out(1, "older"));
    f.runtime.push_log(&id, out(3, "newer"));
    f.followers.resume_after(&cursor(&id, 2, 1));
    f.followers.follow(&id);
    let (marker, _) = f.next().await;
    assert!(marker.marker);
    assert!(
        marker.message.contains("may have been lost"),
        "{}",
        marker.message
    );
    assert_eq!(f.messages(1).await, ["newer"]);
}

#[tokio::test]
async fn lines_written_after_the_clock_stepped_back_are_read() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    f.runtime.push_log(&id, out(1, "read before"));
    f.runtime.push_log(&id, out(10, "the cursor's"));
    // The clock stepped back while hoster was down: later lines carry earlier stamps.
    f.runtime.push_log(&id, out(5, "after the step"));
    f.followers.resume_after(&cursor(&id, 10, 1));
    f.followers.follow(&id);
    assert_eq!(f.messages(1).await, ["after the step"]);
    assert_eq!(f.settle().await, Vec::<String>::new());
}

#[tokio::test]
async fn a_container_that_exited_while_hoster_was_down_is_read_once_to_its_end() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    f.runtime.push_log(&id, out(1, "stored"));
    f.runtime.push_log(&id, out(2, "last words"));
    f.runtime.set_state(&id, ContainerState::Exited(Some(1)));
    f.followers.resume_after(&cursor(&id, 1, 1));
    f.followers.follow(&id);
    assert_eq!(f.messages(1).await, ["last words"]);
    f.followers.drained(&id).await;
    assert!(!f.followers.followed().contains(&id));
    assert!(f.known(&id), "its cursor is kept for a restart");
    // The startup pass starts it again: its follower reads on, not from the start.
    f.runtime.start(&id).await.unwrap();
    f.runtime.push_log(&id, out(3, "back"));
    f.followers.follow(&id);
    assert_eq!(f.messages(1).await, ["back"]);
    assert_eq!(f.settle().await, Vec::<String>::new());
}

#[tokio::test]
async fn a_seeded_cursor_does_not_rewind_what_the_registry_knows() {
    let mut f = Fixture::new();
    let id = f.start(&generation("api", 1)).await;
    f.runtime.push_log(&id, out(1, "one"));
    f.runtime.push_log(&id, out(2, "two"));
    f.followers.follow(&id);
    assert_eq!(f.messages(2).await, ["one", "two"]);
    f.runtime.set_state(&id, ContainerState::Exited(Some(0)));
    f.ended(&id).await;
    f.followers.resume_after(&cursor(&id, 1, 1));
    f.runtime.start(&id).await.unwrap();
    f.followers.follow(&id);
    assert_eq!(f.settle().await, Vec::<String>::new());
}

#[tokio::test]
async fn cursors_of_containers_that_are_gone_are_swept() {
    let dir = TempDir::new().unwrap();
    let store = LogStore::open(dir.path()).await.unwrap();
    let (recorder, sink) = LogRecorder::new(store.clone()).await.unwrap();
    let stop = CancellationToken::new();
    let recording = tokio::spawn(
        recorder
            .with_interval(Duration::from_millis(10))
            .run(stop.clone()),
    );
    let runtime = Arc::new(FakeRuntime::new());
    let f = Fixture {
        runtime: Arc::clone(&runtime),
        followers: LogFollowers::new(Arc::clone(&runtime), sink.clone(), CancellationToken::new()),
        tap: LogSink::tapped(1).1,
    };
    let kept = f.start(&generation("api", 1)).await;
    let removed = f.start(&generation("web", 1)).await;
    let record = |message: &str| LogRecord {
        time: Timestamp::from(at(1)),
        stack: Some("notes".into()),
        service: "api".into(),
        generation: Some(1),
        stream: Stream::Stdout,
        level: None,
        message: message.into(),
        fields: None,
        cut: false,
        marker: false,
        trace_id: None,
        span_id: None,
        received: None,
    };
    // Cursors from before the restart; "gone" was removed while hoster was down.
    let gone = ContainerId::from("gone".to_owned());
    for id in [&kept, &removed, &gone] {
        sink.try_record_line(record("line"), cursor(id, 1, 1));
    }
    let stored = async |n: usize| {
        tokio::time::timeout(WAIT, async {
            while store.cursors().await.unwrap().len() != n {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the cursors are committed");
    };
    stored(3).await;
    assert_eq!(f.followers.resume_from(&store).await.unwrap(), 3);

    // The startup sweep deletes the cursor of the container that is gone.
    f.followers.sweep_hourly(store.clone());
    stored(2).await;
    assert!(!f.known(&gone));
    // A container removed from now on loses its cursor as it is forgotten.
    runtime.remove(&removed).await.unwrap();
    f.followers.forget(&removed);
    stored(1).await;
    assert_eq!(
        store.cursor(kept.as_ref()).await.unwrap(),
        Some(cursor(&kept, 1, 1))
    );
    f.followers.stop().await;
    stop.cancel();
    recording.await.unwrap();
}
