use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use tempfile::TempDir;

use super::*;
use crate::stats::tests::FakeStats;
use crate::stats::{CertExpiry, Stats, StatsSource, Traffic};
use crate::store::{DeploymentOutcome, Store, TrafficCounts};

const SECOND: Duration = Duration::from_secs(1);
const MS: Duration = Duration::from_millis(1);

/// The traffic history of a real store, the way [`super::super::HostStats`] reads it.
struct StoreSource(Store);

#[async_trait]
impl StatsSource for StoreSource {
    async fn deployments(&self) -> anyhow::Result<Vec<DeploymentOutcome>> {
        Ok(Vec::new())
    }

    fn certificates(&self) -> Vec<CertExpiry> {
        Vec::new()
    }

    async fn traffic(
        &self,
        resolution: Resolution,
        span: std::ops::Range<Timestamp>,
    ) -> anyhow::Result<Vec<TrafficBucket<Histogram>>> {
        Ok(self.0.traffic(resolution, span, None).await?)
    }
}

/// A store to put fixture rows into, the [`Stats`] reading it, and a fixed hour to place
/// them around.
struct Fixture {
    _dir: TempDir,
    store: Store,
    stats: Stats,
    /// The start of an hour; the tests read "now" as 30 min 30 s into it.
    hour: Timestamp,
}

impl Fixture {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let stats = Stats::new(Traffic::default(), Arc::new(StoreSource(store.clone())));
        let hour = Timestamp::from(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_002_800));
        assert_eq!(Resolution::Hour.bucket(hour), hour);
        Fixture {
            _dir: dir,
            store,
            stats,
            hour,
        }
    }

    /// 30 min 30 s into the hour: the newest flushed minute is the hour's 30th (`hour + 29
    /// min`).
    fn now(&self) -> Timestamp {
        self.hour + MINUTE * 30 + SECOND * 30
    }

    async fn record(&self, buckets: impl IntoIterator<Item = TrafficBucket<Histogram>>) {
        self.store.record_traffic(buckets).await.unwrap();
    }

    async fn history(&self, range: Range) -> TrafficHistory {
        self.stats.history_at(range, self.now()).await.unwrap()
    }
}

/// `requests` 2xx responses of `latency` each to the stack `blog`'s web route, in the minute of
/// `start`.
fn blog(start: Timestamp, requests: u64, latency: Duration) -> TrafficBucket<Histogram> {
    row(
        start,
        series("blog.example.com", "blog", "web"),
        requests,
        latency,
    )
}

fn series(host: &str, stack: &str, service: &str) -> TrafficSeries {
    TrafficSeries {
        host: host.into(),
        prefix: if host.is_empty() { "" } else { "/" }.into(),
        stack: stack.into(),
        service: service.into(),
    }
}

fn row(
    start: Timestamp,
    series: TrafficSeries,
    requests: u64,
    latency: Duration,
) -> TrafficBucket<Histogram> {
    let mut histogram = Histogram::default();
    for _ in 0..requests {
        histogram.record(latency);
    }
    TrafficBucket {
        start,
        series,
        counts: TrafficCounts {
            s2xx: requests,
            ..TrafficCounts::default()
        },
        latency: histogram,
    }
}

#[test]
fn ranges_read_and_write_as_their_query_values() {
    let values: Vec<String> = Range::ALL.iter().map(Range::to_string).collect();
    assert_eq!(values, ["1h", "6h", "24h", "7d", "30d"]);
    for range in Range::ALL {
        assert_eq!(range.to_string().parse::<Range>(), Ok(range));
    }
    assert_eq!(Range::default(), Range::Day);
    let err = "2h".parse::<Range>().unwrap_err();
    assert!(err.to_string().contains("\"2h\""), "{err}");
}

#[test]
fn every_range_is_covered_by_its_bars() {
    let plans: Vec<_> = Range::ALL
        .iter()
        .map(|r| (r.bars(), r.bar().as_secs() / 60, r.resolution()))
        .collect();
    assert_eq!(
        plans,
        [
            (60, 1, Resolution::Minute),
            (72, 5, Resolution::Minute),
            (24, 60, Resolution::Hour),
            (56, 180, Resolution::Hour),
            (30, 1440, Resolution::Hour),
        ]
    );
    for range in Range::ALL {
        assert_eq!(range.bar() * range.bars() as u32, range.length(), "{range}");
    }
}

#[tokio::test]
async fn a_fresh_install_has_empty_bars_and_no_figures() {
    let fixture = Fixture::new().await;
    for range in Range::ALL {
        let history = fixture.history(range).await;
        for scope in [history.all(), history.stack("blog"), history.unrouted()] {
            assert_eq!(scope.bars.len(), range.bars(), "{range}");
            assert!(scope.bars.iter().all(|bar| bar.traffic.requests() == 0));
            assert_eq!(scope.totals, TrafficTotals::default());
            assert_eq!((scope.totals.p50(), scope.totals.p95()), (None, None));
            assert_eq!(scope.totals.server_error_share(), None);
            assert_eq!(scope.previous, None);
            assert_eq!((scope.recent, scope.per_minute()), (0, 0.0));
            assert_eq!(scope.peak, None);
        }
        assert_eq!(history.stacks().count(), 0);
        assert_eq!(history.range(), range);
    }
}

#[tokio::test]
async fn minute_ranges_end_with_the_newest_flushed_minute() {
    let fixture = Fixture::new().await;
    let flushed = fixture.hour + MINUTE * 30;
    fixture
        .record([
            // The live minute is never read.
            blog(flushed, 1000, MS),
            // The newest flushed minute: the last bar of both ranges.
            blog(flushed - MINUTE, 1, MS),
            // The first minute of the 1h range, the 6h range's 61st bar.
            blog(flushed - MINUTE * 60, 2, MS),
            // The last minute of the period before 1h.
            blog(flushed - MINUTE * 61, 4, MS),
            // The first minute of the 6h range, the last before it.
            blog(flushed - MINUTE * 360, 8, MS),
            blog(flushed - MINUTE * 361, 16, MS),
        ])
        .await;

    let hour = fixture.history(Range::Hour).await;
    assert_eq!(hour.end(), flushed);
    let bars = &hour.all().bars;
    assert_eq!(bars[0].start, flushed - MINUTE * 60);
    assert_eq!(bars[0].traffic.requests(), 2);
    assert_eq!(bars[59].traffic.requests(), 1);
    assert_eq!(hour.all().totals.requests(), 3);
    assert_eq!(hour.all().previous.unwrap().requests(), 4);

    let six = fixture.history(Range::SixHours).await;
    let bars = &six.all().bars;
    assert_eq!(bars.len(), 72);
    assert_eq!(bars[0].start, flushed - MINUTE * 360);
    assert_eq!(bars[0].traffic.requests(), 8);
    // Bars of 5 min: the 1h range's first minute starts the 61st.
    assert_eq!(bars[60].start, flushed - MINUTE * 60);
    assert_eq!(bars[60].traffic.requests(), 2);
    assert_eq!(bars[71].traffic.requests(), 1);
    assert_eq!(six.all().totals.requests(), 1 + 2 + 4 + 8);
    assert_eq!(six.all().previous.unwrap().requests(), 16);
}

#[tokio::test]
async fn a_minute_is_read_once_its_flush_is_due() {
    let fixture = Fixture::new().await;
    let minute = fixture.hour + MINUTE * 30;
    fixture.record([blog(minute, 3, MS)]).await;
    // Two seconds after the minute ends its flush may still be under way.
    let early = fixture
        .stats
        .history_at(Range::Hour, minute + MINUTE + SECOND * 2)
        .await
        .unwrap();
    assert_eq!(early.all().totals.requests(), 0);
    let late = fixture
        .stats
        .history_at(Range::Hour, minute + MINUTE + SECOND * 10)
        .await
        .unwrap();
    assert_eq!(late.all().totals.requests(), 3);
    assert_eq!(late.all().bars[59].start, minute);
}

#[tokio::test]
async fn hour_ranges_end_with_the_partial_hour() {
    let fixture = Fixture::new().await;
    let hour = fixture.hour;
    let end = hour + HOUR;
    fixture
        .record([
            // In the current, partial hour.
            blog(hour + MINUTE * 29, 1, MS),
            blog(hour - HOUR * 23, 2, MS),
            // Before the 24h range, in the 7d range's last bar but eight.
            blog(hour - HOUR * 24, 4, MS),
            blog(end - Range::Week.length(), 8, MS),
            // Before the 7d range.
            blog(end - DAY * 10, 16, MS),
            // Before the 30d range.
            blog(end - DAY * 31, 32, MS),
        ])
        .await;

    let day = fixture.history(Range::Day).await;
    assert_eq!(day.end(), end);
    let bars = &day.all().bars;
    assert_eq!(bars[0].start, hour - HOUR * 23);
    assert_eq!(bars[0].traffic.requests(), 2);
    assert_eq!(bars[23].start, hour);
    assert_eq!(bars[23].traffic.requests(), 1);
    assert_eq!(day.all().totals.requests(), 3);
    assert_eq!(day.all().previous.unwrap().requests(), 4);

    let week = fixture.history(Range::Week).await;
    let bars = &week.all().bars;
    assert_eq!(bars[0].traffic.requests(), 8);
    // 24 h before the end of the range is the start of the 49th of 56 bars of 3 h.
    assert_eq!(bars[48].start, hour - HOUR * 23);
    assert_eq!(bars[47].traffic.requests(), 4);
    assert_eq!(bars[48].traffic.requests(), 2);
    assert_eq!(bars[55].traffic.requests(), 1);
    assert_eq!(week.all().previous.unwrap().requests(), 16);

    let month = fixture.history(Range::ThirtyDays).await;
    let bars = &month.all().bars;
    assert_eq!(bars[0].start, end - DAY * 30);
    assert_eq!(bars[20].traffic.requests(), 16);
    assert_eq!(bars[23].traffic.requests(), 8);
    assert_eq!(bars[28].traffic.requests(), 4);
    assert_eq!(bars[29].traffic.requests(), 1 + 2);
    assert_eq!(month.all().totals.requests(), 1 + 2 + 4 + 8 + 16);
    assert_eq!(month.all().previous.unwrap().requests(), 32);
    assert_eq!(month.all().peak, None);
}

#[tokio::test]
async fn the_previous_period_without_rows_has_no_totals() {
    let fixture = Fixture::new().await;
    fixture.record([blog(fixture.hour, 5, MS)]).await;
    let history = fixture.history(Range::Hour).await;
    assert_eq!(history.all().totals.requests(), 5);
    assert_eq!(history.all().previous, None);
}

#[tokio::test]
async fn the_rate_covers_the_last_five_flushed_minutes_whatever_the_range() {
    let fixture = Fixture::new().await;
    let flushed = fixture.hour + MINUTE * 30;
    fixture
        .record([
            blog(flushed - MINUTE, 10, MS),
            blog(flushed - MINUTE * 5, 5, MS),
            // Just before the rate window.
            blog(flushed - MINUTE * 6, 100, MS),
            // The live minute.
            blog(flushed, 1000, MS),
        ])
        .await;
    for range in Range::ALL {
        let history = fixture.history(range).await;
        assert_eq!(history.all().recent, 15, "{range}");
        assert_eq!(history.all().per_minute(), 3.0, "{range}");
        assert_eq!(history.stack("blog").per_minute(), 3.0, "{range}");
    }
}

#[tokio::test]
async fn the_peak_minute_adds_up_routes_and_only_minute_ranges_have_one() {
    let fixture = Fixture::new().await;
    let flushed = fixture.hour + MINUTE * 30;
    let api = series("api.example.com", "blog", "api");
    fixture
        .record([
            blog(flushed - MINUTE * 50, 30, MS),
            // The busiest minute, over two routes.
            blog(flushed - MINUTE * 20, 20, MS),
            row(flushed - MINUTE * 20, api.clone(), 20, MS),
            // As busy as the earlier one: the later wins.
            blog(flushed - MINUTE * 10, 40, MS),
            // Busier, but before the range.
            blog(flushed - MINUTE * 70, 500, MS),
        ])
        .await;

    let hour = fixture.history(Range::Hour).await;
    let peak = PeakMinute {
        at: flushed - MINUTE * 10,
        requests: 40,
    };
    assert_eq!(hour.all().peak, Some(peak));
    assert_eq!(hour.stack("blog").peak, Some(peak));
    let six = fixture.history(Range::SixHours).await;
    assert_eq!(
        six.all().peak,
        Some(PeakMinute {
            at: flushed - MINUTE * 70,
            requests: 500,
        })
    );
    assert_eq!(fixture.history(Range::Day).await.all().peak, None);
}

#[tokio::test]
async fn all_hosted_traffic_leaves_out_hosters_own_routes() {
    let fixture = Fixture::new().await;
    let minute = fixture.hour + MINUTE * 10;
    fixture
        .record([
            blog(minute, 1, MS),
            row(minute, series("shop.example.com", "shop", "web"), 2, MS),
            // A stack since removed: no stack row, but part of all hosted traffic.
            row(minute, series("old.example.com", "old", "web"), 4, MS),
            row(minute, series("", "", ""), 8, MS),
            row(minute, series("admin.example.com", "", "admin UI"), 16, MS),
        ])
        .await;
    for range in Range::ALL {
        let history = fixture.history(range).await;
        assert_eq!(history.all().totals.requests(), 1 + 2 + 4 + 8, "{range}");
        assert_eq!(history.unrouted().totals.requests(), 8);
        let stacks: Vec<_> = history
            .stacks()
            .map(|(name, scope)| (name, scope.totals.requests()))
            .collect();
        assert_eq!(stacks, [("blog", 1), ("old", 4), ("shop", 2)]);
        let reserved: Vec<_> = history
            .reserved()
            .map(|(purpose, scope)| (purpose, scope.totals.requests()))
            .collect();
        assert_eq!(reserved, [("admin UI", 16)]);
        assert_eq!(history.stack("gone").totals.requests(), 0);
        assert_eq!(history.stack("gone").bars.len(), range.bars());
    }
}

#[tokio::test]
async fn percentiles_merge_across_routes_and_minutes() {
    let fixture = Fixture::new().await;
    let minute = fixture.hour + MINUTE * 10;
    let api = series("api.example.com", "blog", "api");
    let mut slow = row(minute + MINUTE, api, 10, Duration::from_secs(2));
    slow.counts = TrafficCounts {
        s2xx: 4,
        s4xx: 1,
        s5xx: 5,
        upstream_errors: 3,
        ..TrafficCounts::default()
    };
    fixture.record([blog(minute, 90, MS), slow]).await;
    for range in Range::ALL {
        let history = fixture.history(range).await;
        let totals = history.stack("blog").totals;
        assert_eq!(totals.requests(), 100, "{range}");
        // 90 fast and 10 slow requests: the median is fast, the p95 slow.
        let p50 = totals.p50().unwrap();
        assert!(p50 <= MS * 2, "{range}: {p50:?}");
        let p95 = totals.p95().unwrap();
        assert!(p95 >= Duration::from_secs(2), "{range}: {p95:?}");
        assert_eq!((totals.client_errors(), totals.server_errors()), (1, 5));
        assert_eq!(totals.server_error_share(), Some(0.05));
        assert_eq!(totals.client_error_share(), Some(0.01));
        assert_eq!(totals.upstream_errors, 3);
    }
    // In the 1h range the two minutes are bars of their own.
    let hour = fixture.history(Range::Hour).await;
    let bars = &hour.all().bars;
    let [fast, slow] = [&bars[40], &bars[41]];
    assert_eq!(fast.start, minute);
    assert!(fast.traffic.p95().unwrap() <= MS * 2);
    assert!(slow.traffic.p50().unwrap() >= Duration::from_secs(2));
}

#[tokio::test]
async fn the_test_fake_rolls_its_minutes_up_into_hours() {
    let fake = Arc::new(FakeStats::default());
    let now = Timestamp::now();
    let flushed = Resolution::Minute.bucket(now - Plan::FLUSH_LAG);
    fake.traffic.lock().unwrap().extend([
        blog(flushed - MINUTE, 3, MS),
        blog(flushed - MINUTE * 2, 2, MS),
        blog(flushed - HOUR * 2, 1, MS),
    ]);
    let stats = Stats::new(Traffic::default(), fake);
    let day = stats.history_at(Range::Day, now).await.unwrap();
    assert_eq!(day.all().totals.requests(), 6);
    assert_eq!(day.all().recent, 5);
    let hour = stats.history_at(Range::Hour, now).await.unwrap();
    assert_eq!(hour.all().totals.requests(), 5);
    assert_eq!(hour.all().bars[59].traffic.requests(), 3);
}
