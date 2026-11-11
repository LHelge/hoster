//! The traffic recorder (ADR-0062 §7): turns the proxy's in-memory counters into traffic
//! history.
//!
//! Just after every wall-clock minute it drains [`Traffic`] and writes what each route counted
//! under the minute that ended, together with its hour rollup, in one store transaction. It
//! sweeps both tables to their retention at startup and hourly, and flushes the partial minute
//! once more when it is stopped, which the supervisor does after the proxy has stopped taking
//! requests (docs/architecture.md §4).

use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::{Histogram, StatusClass, Traffic, TrafficDelta};
use crate::routing::Owner;
use crate::store::{Resolution, Store, Timestamp, TrafficBucket, TrafficCounts, TrafficSeries};

/// Where the recorder's time comes from: the system clock in production, a hand-driven one in
/// tests.
pub trait Clock: Send + Sync + 'static {
    /// The current wall-clock time.
    fn now(&self) -> Timestamp;

    /// Resolves once the clock reads `at` or later.
    fn sleep_until(&self, at: Timestamp) -> impl Future<Output = ()> + Send;
}

/// The system's wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }

    async fn sleep_until(&self, at: Timestamp) {
        let millis = at.as_millis().saturating_sub(self.now().as_millis());
        tokio::time::sleep(Duration::from_millis(u64::try_from(millis).unwrap_or(0))).await;
    }
}

/// Drains [`Traffic`] once a minute into the store's traffic history (ADR-0062).
pub struct TrafficRecorder<C = SystemClock> {
    traffic: Traffic,
    store: Store,
    clock: C,
    /// Buckets a failed write left, tried again with the next flush until they are older
    /// than [`Self::RETRY_WINDOW`].
    unwritten: Vec<TrafficBucket<Histogram>>,
}

impl TrafficRecorder {
    /// A recorder of `traffic` into `store` on the system clock.
    pub fn new(traffic: Traffic, store: Store) -> Self {
        Self::with_clock(traffic, store, SystemClock)
    }
}

impl<C: Clock> TrafficRecorder<C> {
    /// How long minute buckets are kept (ADR-0062 §2).
    pub const MINUTE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
    /// How long hour buckets are kept (ADR-0062 §2).
    pub const HOUR_RETENTION: Duration = Duration::from_secs(60 * 24 * 60 * 60);
    /// How often the retention sweep runs after the one at startup.
    const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
    /// How long a bucket whose write failed is tried again before it is dropped.
    const RETRY_WINDOW: Duration = Duration::from_secs(5 * 60);
    /// How long after a minute ends its tick runs, so a tick never lands just before the
    /// minute it writes has ended.
    const TICK_DELAY: Duration = Duration::from_secs(1);

    /// A recorder of `traffic` into `store` on `clock`.
    pub fn with_clock(traffic: Traffic, store: Store, clock: C) -> Self {
        Self {
            traffic,
            store,
            clock,
            unwritten: Vec::new(),
        }
    }

    /// Sweeps the history to its retention, then flushes just after every wall-clock minute
    /// and sweeps hourly until `cancel` fires; then flushes the partial minute and returns.
    /// Failed writes and sweeps are logged and never end the loop.
    pub async fn run(mut self, cancel: CancellationToken) {
        self.sweep().await;
        let mut next_sweep = self.clock.now() + Self::SWEEP_INTERVAL;
        loop {
            let minute = Resolution::Minute.bucket(self.clock.now());
            let tick = minute + Resolution::Minute.length() + Self::TICK_DELAY;
            tokio::select! {
                () = cancel.cancelled() => break,
                () = self.clock.sleep_until(tick) => {}
            }
            // The minute that ended last: a late tick writes everything since the previous
            // one under it, so it still writes one bucket and never one twice.
            let now = self.clock.now();
            let ended = Resolution::Minute.bucket(now) - Resolution::Minute.length();
            self.flush(ended).await;
            if now >= next_sweep {
                self.sweep().await;
                next_sweep = now + Self::SWEEP_INTERVAL;
            }
        }
        // The partial minute; a restart within it adds to the same bucket.
        let minute = Resolution::Minute.bucket(self.clock.now());
        self.flush(minute).await;
        if !self.unwritten.is_empty() {
            tracing::warn!(
                buckets = self.unwritten.len(),
                "traffic history not written at shutdown; dropping it"
            );
        }
        tracing::debug!("traffic recorder stopped");
    }

    /// Drains the counters and writes them, with any buckets left by a failed write, under
    /// `minute`. A failed write keeps the buckets of the last [`Self::RETRY_WINDOW`] for the
    /// next flush and drops older ones.
    async fn flush(&mut self, minute: Timestamp) {
        let mut buckets = std::mem::take(&mut self.unwritten);
        buckets.extend(
            self.traffic
                .drain()
                .into_iter()
                .map(|delta| TrafficBucket::from((minute, delta))),
        );
        if buckets.is_empty() {
            return;
        }
        let Err(err) = self.store.record_traffic(buckets.clone()).await else {
            return;
        };
        let oldest = minute - Self::RETRY_WINDOW;
        let before = buckets.len();
        buckets.retain(|bucket| bucket.start >= oldest);
        tracing::warn!(
            error = format!("{:#}", anyhow::Error::from(err)),
            kept = buckets.len(),
            dropped = before - buckets.len(),
            "cannot write traffic history; trying again with the next flush"
        );
        self.unwritten = buckets;
    }

    /// Deletes the minute and hour buckets older than their retention.
    async fn sweep(&self) {
        let now = self.clock.now();
        for (resolution, retention) in [
            (Resolution::Minute, Self::MINUTE_RETENTION),
            (Resolution::Hour, Self::HOUR_RETENTION),
        ] {
            match self.store.prune_traffic(resolution, now - retention).await {
                Ok(0) => {}
                Ok(deleted) => tracing::debug!(?resolution, deleted, "pruned traffic history"),
                Err(err) => tracing::warn!(
                    error = format!("{:#}", anyhow::Error::from(err)),
                    ?resolution,
                    "cannot prune traffic history; trying again in an hour"
                ),
            }
        }
    }
}

/// What `delta` counted, as the bucket of `start` (ADR-0062 §4): hoster's own routes have no
/// stack and their purpose as the service, unrouted requests no host, prefix or owner.
impl From<(Timestamp, TrafficDelta)> for TrafficBucket<Histogram> {
    fn from((start, delta): (Timestamp, TrafficDelta)) -> Self {
        let (stack, service) = match delta.owner {
            Some(Owner::Route(owner)) => (owner.stack.to_string(), owner.service.to_string()),
            Some(Owner::Reserved(purpose)) => (String::new(), purpose.to_owned()),
            None => (String::new(), String::new()),
        };
        let count = |class| delta.statuses.get(class);
        TrafficBucket {
            start,
            series: TrafficSeries {
                host: delta.host,
                prefix: delta.prefix,
                stack,
                service,
            },
            counts: TrafficCounts {
                none: count(StatusClass::NoResponse),
                s1xx: count(StatusClass::Informational),
                s2xx: count(StatusClass::Success),
                s3xx: count(StatusClass::Redirection),
                s4xx: count(StatusClass::ClientError),
                s5xx: count(StatusClass::ServerError),
                upstream_errors: delta.upstream_errors,
            },
            latency: delta.latency,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::SqlitePool;
    use sqlx::sqlite::SqliteConnectOptions;
    use tempfile::TempDir;
    use tokio::sync::watch;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::routing::{RouteEntry, RouteOwner, Target};
    use crate::stack::RouteTimeouts;
    use crate::stats::{RequestOutcome, StatusCounts};

    const SECOND: Duration = Duration::from_secs(1);
    const MINUTE: Duration = Duration::from_secs(60);
    const HOUR: Duration = Duration::from_secs(60 * 60);
    const HOST: &str = "app.example.com";

    /// A clock the test moves by hand. It knows what the recorder sleeps until, so the test can
    /// wait for a tick to have run.
    #[derive(Clone)]
    struct ManualClock {
        now: Arc<watch::Sender<Timestamp>>,
        sleeping_until: Arc<watch::Sender<Option<Timestamp>>>,
    }

    impl ManualClock {
        fn new(now: Timestamp) -> Self {
            Self {
                now: Arc::new(watch::Sender::new(now)),
                sleeping_until: Arc::new(watch::Sender::new(None)),
            }
        }

        /// Waits until the recorder sleeps until a time still ahead: everything due by now
        /// has run.
        async fn settled(&self) {
            let now = self.now();
            let mut sleeping = self.sleeping_until.subscribe();
            let waiting = sleeping.wait_for(|until| until.is_some_and(|until| until > now));
            tokio::time::timeout(Duration::from_secs(5), waiting)
                .await
                .expect("the recorder goes back to sleep")
                .unwrap();
        }

        /// Moves the clock to `at` and waits for the ticks due by then to run.
        async fn advance_to(&self, at: Timestamp) {
            self.now.send_replace(at);
            self.settled().await;
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Timestamp {
            *self.now.borrow()
        }

        async fn sleep_until(&self, at: Timestamp) {
            let mut now = self.now.subscribe();
            self.sleeping_until.send_replace(Some(at));
            let _ = now.wait_for(|now| *now >= at).await;
        }
    }

    /// A recorder on a fresh store, running from 30 s into an hour (returned as `hour`).
    struct Harness {
        _dir: TempDir,
        store: Store,
        traffic: Traffic,
        clock: ManualClock,
        hour: Timestamp,
        cancel: CancellationToken,
        task: JoinHandle<()>,
    }

    impl Harness {
        async fn start() -> Self {
            let dir = TempDir::new().unwrap();
            let store = Store::open(dir.path()).await.unwrap();
            Self::start_on(dir, store).await
        }

        async fn start_on(dir: TempDir, store: Store) -> Self {
            let hour = Self::hour();
            let traffic = Traffic::default();
            let clock = ManualClock::new(hour + 30 * SECOND);
            let cancel = CancellationToken::new();
            let recorder =
                TrafficRecorder::with_clock(traffic.clone(), store.clone(), clock.clone());
            let task = tokio::spawn(recorder.run(cancel.clone()));
            clock.settled().await;
            Self {
                _dir: dir,
                store,
                traffic,
                clock,
                hour,
                cancel,
                task,
            }
        }

        /// The start of the current hour, where every test's clock starts.
        fn hour() -> Timestamp {
            Resolution::Hour.bucket(Timestamp::now())
        }

        /// `count` successful requests to `/` of stack `site`.
        fn requests(&self, count: usize) {
            let route = RouteEntry {
                owner: Owner::Route(RouteOwner {
                    stack: "site".parse().unwrap(),
                    service: "web".parse().unwrap(),
                }),
                prefix: "/".parse().unwrap(),
                prefix_str: "/".to_owned(),
                strip_prefix: false,
                target: Target::Unavailable {
                    retry_after: SECOND,
                },
                timeouts: RouteTimeouts::default(),
            };
            for _ in 0..count {
                self.traffic.route(HOST, &route).finish(RequestOutcome {
                    status: Some(200),
                    latency: Duration::from_millis(5),
                    upstream_error: false,
                });
            }
        }

        /// Moves the clock to `offset` past the hour, running the ticks due.
        async fn at(&self, offset: Duration) {
            self.clock.advance_to(self.hour + offset).await;
        }

        /// Every `resolution` bucket's start (as the offset from the hour) and its 2xx count.
        async fn rows(&self, resolution: Resolution) -> Vec<(i64, u64)> {
            let all = Timestamp::from(std::time::UNIX_EPOCH)..self.hour + 1000 * HOUR;
            let buckets: Vec<TrafficBucket<Histogram>> =
                self.store.traffic(resolution, all, None).await.unwrap();
            buckets
                .into_iter()
                .map(|b| {
                    let offset = b.start.as_millis() - self.hour.as_millis();
                    (offset / 1000, b.counts.s2xx)
                })
                .collect()
        }

        /// Cancels the recorder and waits for its last flush.
        async fn stop(&mut self) {
            self.cancel.cancel();
            tokio::time::timeout(Duration::from_secs(5), &mut self.task)
                .await
                .expect("cancel stops the recorder")
                .unwrap();
        }

        /// Renames the minute table away (`broken`) or back, so writes fail or work again.
        async fn break_writes(&self, broken: bool) {
            let pool =
                SqlitePool::connect_with(SqliteConnectOptions::new().filename(self.store.path()))
                    .await
                    .unwrap();
            let sql = if broken {
                "ALTER TABLE traffic_minutes RENAME TO traffic_minutes_gone"
            } else {
                "ALTER TABLE traffic_minutes_gone RENAME TO traffic_minutes"
            };
            sqlx::query(sql).execute(&pool).await.unwrap();
            pool.close().await;
        }
    }

    #[tokio::test]
    async fn each_tick_writes_the_minute_that_ended_and_idle_minutes_write_nothing() {
        let mut h = Harness::start().await;
        h.requests(3);
        h.at(61 * SECOND).await;
        assert_eq!(h.rows(Resolution::Minute).await, [(0, 3)]);
        assert_eq!(h.rows(Resolution::Hour).await, [(0, 3)]);

        h.requests(2);
        h.at(121 * SECOND).await;
        assert_eq!(h.rows(Resolution::Minute).await, [(0, 3), (60, 2)]);

        // An idle minute, then one with a request.
        h.at(181 * SECOND).await;
        h.requests(1);
        h.at(241 * SECOND).await;
        assert_eq!(
            h.rows(Resolution::Minute).await,
            [(0, 3), (60, 2), (180, 1)]
        );
        assert_eq!(h.rows(Resolution::Hour).await, [(0, 6)]);

        let buckets: Vec<TrafficBucket<Histogram>> = h
            .store
            .traffic(Resolution::Minute, h.hour..h.hour + MINUTE, None)
            .await
            .unwrap();
        assert_eq!(
            buckets[0].series,
            TrafficSeries {
                host: HOST.into(),
                prefix: "/".into(),
                stack: "site".into(),
                service: "web".into(),
            }
        );
        assert_eq!(buckets[0].latency.samples(), 3);
        h.stop().await;
    }

    #[tokio::test]
    async fn a_late_tick_writes_one_bucket() {
        let mut h = Harness::start().await;
        h.requests(4);
        h.at(5 * MINUTE + SECOND).await;
        assert_eq!(h.rows(Resolution::Minute).await, [(240, 4)]);
        h.stop().await;
    }

    #[tokio::test]
    async fn cancel_flushes_the_partial_minute() {
        let mut h = Harness::start().await;
        h.requests(1);
        h.at(61 * SECOND).await;
        h.requests(2);
        h.at(90 * SECOND).await;
        h.stop().await;
        assert_eq!(h.rows(Resolution::Minute).await, [(0, 1), (60, 2)]);
    }

    #[tokio::test]
    async fn a_failed_write_is_retried_with_the_next_flush() {
        let mut h = Harness::start().await;
        h.break_writes(true).await;
        h.requests(1);
        h.at(61 * SECOND).await;
        h.break_writes(false).await;
        assert_eq!(h.rows(Resolution::Minute).await, []);

        h.requests(2);
        h.at(121 * SECOND).await;
        assert_eq!(h.rows(Resolution::Minute).await, [(0, 1), (60, 2)]);
        assert_eq!(h.rows(Resolution::Hour).await, [(0, 3)]);
        h.stop().await;
    }

    #[tokio::test]
    async fn buckets_failing_for_longer_than_the_retry_window_are_dropped() {
        let mut h = Harness::start().await;
        h.break_writes(true).await;
        h.requests(1);
        h.at(61 * SECOND).await;
        h.requests(2);
        h.at(121 * SECOND).await;
        // After this flush fails, minute 0 is older than the retry window; minute 1 is not.
        h.at(7 * MINUTE + SECOND).await;
        h.break_writes(false).await;
        h.at(8 * MINUTE + SECOND).await;
        assert_eq!(h.rows(Resolution::Minute).await, [(60, 2)]);
        h.stop().await;
    }

    #[tokio::test]
    async fn retention_is_swept_at_startup_and_then_hourly() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let now = Harness::hour() + 30 * SECOND;
        let bucket = |start: Timestamp| TrafficBucket {
            start,
            series: TrafficSeries::default(),
            counts: TrafficCounts {
                s2xx: 1,
                ..TrafficCounts::default()
            },
            latency: Histogram::default(),
        };
        let day = 24 * HOUR;
        store
            .record_traffic([
                bucket(now - 61 * day),
                bucket(now - day - HOUR),
                bucket(now - day + 30 * MINUTE),
            ])
            .await
            .unwrap();

        let mut h = Harness::start_on(dir, store).await;
        let starts = |rows: Vec<(i64, u64)>| -> Vec<i64> {
            rows.into_iter().map(|(start, _)| start).collect()
        };
        // Minute rows older than a day and hour rows older than 60 days are gone.
        assert_eq!(
            starts(h.rows(Resolution::Minute).await),
            [-(23 * 3600 + 30 * 60)]
        );
        assert_eq!(
            starts(h.rows(Resolution::Hour).await),
            [-25 * 3600, -24 * 3600]
        );

        // An hour later the remaining minute row has passed the retention too.
        h.at(HOUR + MINUTE + SECOND).await;
        assert_eq!(h.rows(Resolution::Minute).await, []);
        assert_eq!(
            starts(h.rows(Resolution::Hour).await),
            [-25 * 3600, -24 * 3600]
        );
        h.stop().await;
    }

    #[test]
    fn deltas_become_buckets_of_their_series() {
        let start = Harness::hour();
        let unrouted = TrafficDelta {
            host: String::new(),
            prefix: String::new(),
            owner: None,
            statuses: StatusCounts::default(),
            upstream_errors: 0,
            latency: Histogram::default(),
        };
        let reserved = TrafficDelta {
            host: "admin.example.com".into(),
            prefix: "/".into(),
            owner: Some(Owner::Reserved("admin UI")),
            ..unrouted.clone()
        };
        let bucket = TrafficBucket::from((start, reserved));
        assert_eq!(bucket.start, start);
        assert_eq!(
            bucket.series,
            TrafficSeries {
                host: "admin.example.com".into(),
                prefix: "/".into(),
                stack: String::new(),
                service: "admin UI".into(),
            }
        );
        let unrouted = TrafficBucket::from((start, unrouted));
        assert_eq!(unrouted.series, TrafficSeries::default());
    }
}
