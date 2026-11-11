//! Traffic history (ADR-0062): per-route request counts and latency histograms in 1-minute
//! buckets (`traffic_minutes`) and 1-hour buckets (`traffic_hours`).
//!
//! [`Store::record_traffic`] is the recorder's one write: it adds a flush's minute rows to their
//! minute and hour buckets in one transaction, so the two tables never disagree.
//! [`Store::traffic`] reads a span back and [`Store::prune_traffic`] is the retention sweep.
//!
//! The store merges latency histograms but reads nothing else in them. It names the histogram
//! only through the [`Latencies`] bound, because `stats`, whose `Histogram` is the one
//! implementation, depends on the store.

use std::collections::BTreeMap;
use std::error::Error;
use std::ops::{AddAssign, Range};
use std::time::Duration;

use sqlx::SqliteConnection;

use super::{Result, Store, StoreError, Timestamp};

/// A latency histogram as the traffic tables keep it: histograms add up (`+=`) and convert to
/// and from the bytes stored. Implemented for every type that can, `stats::Histogram` among
/// them.
pub trait Latencies:
    Default
    + Clone
    + AddAssign
    + Into<Vec<u8>>
    + TryFrom<Vec<u8>, Error: Error + Send + Sync + 'static>
    + Send
{
}

impl<L> Latencies for L where
    L: Default
        + Clone
        + AddAssign
        + Into<Vec<u8>>
        + TryFrom<Vec<u8>, Error: Error + Send + Sync + 'static>
        + Send
{
}

/// The bucket size of a traffic table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resolution {
    /// `traffic_minutes`.
    Minute,
    /// `traffic_hours`.
    Hour,
}

impl Resolution {
    /// How long one bucket is.
    pub const fn length(self) -> Duration {
        match self {
            Resolution::Minute => Duration::from_secs(60),
            Resolution::Hour => Duration::from_secs(60 * 60),
        }
    }

    /// The start of the bucket that holds `at`, aligned to a UTC minute or hour.
    pub fn bucket(self, at: Timestamp) -> Timestamp {
        at.floor(self.length())
    }
}

/// Which series a traffic row counts: a route and its owner, `''` for none (ADR-0062 §4).
/// Hoster's own routes have no stack and the reserved purpose as the service; unrouted requests
/// are the series with every field empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrafficSeries {
    /// The route's host as the route table spells it.
    pub host: String,
    /// The route's prefix, e.g. `/` or `/api`.
    pub prefix: String,
    pub stack: String,
    pub service: String,
}

/// What a traffic bucket counts besides latency: finished requests by status class, and those
/// that failed because of their upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrafficCounts {
    /// No response header went out.
    pub none: u64,
    pub s1xx: u64,
    pub s2xx: u64,
    pub s3xx: u64,
    pub s4xx: u64,
    pub s5xx: u64,
    pub upstream_errors: u64,
}

impl AddAssign for TrafficCounts {
    fn add_assign(&mut self, other: TrafficCounts) {
        self.none = self.none.saturating_add(other.none);
        self.s1xx = self.s1xx.saturating_add(other.s1xx);
        self.s2xx = self.s2xx.saturating_add(other.s2xx);
        self.s3xx = self.s3xx.saturating_add(other.s3xx);
        self.s4xx = self.s4xx.saturating_add(other.s4xx);
        self.s5xx = self.s5xx.saturating_add(other.s5xx);
        self.upstream_errors = self.upstream_errors.saturating_add(other.upstream_errors);
    }
}

/// One series' traffic over one bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficBucket<L> {
    /// The bucket start, aligned to its [`Resolution`].
    pub start: Timestamp,
    pub series: TrafficSeries,
    pub counts: TrafficCounts,
    /// The latencies of the requests counted.
    pub latency: L,
}

impl<L: AddAssign> AddAssign for TrafficBucket<L> {
    /// Adds the counts and merges the latencies of `other`, a bucket of the same series.
    fn add_assign(&mut self, other: TrafficBucket<L>) {
        self.counts += other.counts;
        self.latency += other.latency;
    }
}

/// A traffic row as SQLite holds it: counts as `INTEGER`, the histogram encoded.
struct TrafficRow {
    start: Timestamp,
    host: String,
    prefix: String,
    stack: String,
    service: String,
    none: i64,
    s1xx: i64,
    s2xx: i64,
    s3xx: i64,
    s4xx: i64,
    s5xx: i64,
    upstream_errors: i64,
    latency: Vec<u8>,
}

impl<L: Latencies> From<TrafficBucket<L>> for TrafficRow {
    fn from(bucket: TrafficBucket<L>) -> Self {
        let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        let TrafficBucket {
            start,
            series,
            counts,
            latency,
        } = bucket;
        TrafficRow {
            start,
            host: series.host,
            prefix: series.prefix,
            stack: series.stack,
            service: series.service,
            none: count(counts.none),
            s1xx: count(counts.s1xx),
            s2xx: count(counts.s2xx),
            s3xx: count(counts.s3xx),
            s4xx: count(counts.s4xx),
            s5xx: count(counts.s5xx),
            upstream_errors: count(counts.upstream_errors),
            latency: latency.into(),
        }
    }
}

impl<L: Latencies> TryFrom<TrafficRow> for TrafficBucket<L> {
    type Error = StoreError;

    fn try_from(row: TrafficRow) -> Result<Self> {
        let count = |n: i64| u64::try_from(n).unwrap_or(0);
        let latency = L::try_from(row.latency).map_err(|source| StoreError::Latency {
            start: row.start,
            source: Box::new(source),
        })?;
        Ok(TrafficBucket {
            start: row.start,
            series: TrafficSeries {
                host: row.host,
                prefix: row.prefix,
                stack: row.stack,
                service: row.service,
            },
            counts: TrafficCounts {
                none: count(row.none),
                s1xx: count(row.s1xx),
                s2xx: count(row.s2xx),
                s3xx: count(row.s3xx),
                s4xx: count(row.s4xx),
                s5xx: count(row.s5xx),
                upstream_errors: count(row.upstream_errors),
            },
            latency,
        })
    }
}

/// Buckets of one resolution keyed by start and series, each series once per start.
struct Buckets<L>(BTreeMap<(Timestamp, TrafficSeries), TrafficBucket<L>>);

impl<L: Latencies> Buckets<L> {
    /// `buckets` aligned to `resolution`, those that fall into the same bucket added up.
    fn new(resolution: Resolution, buckets: impl IntoIterator<Item = TrafficBucket<L>>) -> Self {
        let mut merged = Buckets(BTreeMap::new());
        for mut bucket in buckets {
            bucket.start = resolution.bucket(bucket.start);
            merged.add(bucket);
        }
        merged
    }

    fn add(&mut self, bucket: TrafficBucket<L>) {
        let key = (bucket.start, bucket.series.clone());
        match self.0.get_mut(&key) {
            Some(sum) => *sum += bucket,
            None => {
                self.0.insert(key, bucket);
            }
        }
    }

    /// Every bucket start, once.
    fn starts(&self) -> Vec<Timestamp> {
        let mut starts: Vec<_> = self.0.keys().map(|(start, _)| *start).collect();
        starts.dedup();
        starts
    }
}

impl Store {
    /// Adds `buckets` (what each series counted, by the minute it counted in) to their minute
    /// rows and their hour rows, in one transaction: a crash leaves both tables with them or
    /// both without. A bucket's start is aligned down to its minute and hour, and a bucket
    /// already stored is added to (counts added, histograms merged), so writing a minute again
    /// adds to it and never duplicates it.
    pub async fn record_traffic<L: Latencies>(
        &self,
        buckets: impl IntoIterator<Item = TrafficBucket<L>>,
    ) -> Result<()> {
        let minutes = Buckets::new(Resolution::Minute, buckets);
        if minutes.0.is_empty() {
            return Ok(());
        }
        let hours = Buckets::new(Resolution::Hour, minutes.0.values().cloned());
        let mut tx = self.begin().await?;
        Self::add_traffic(&mut tx, Resolution::Minute, minutes).await?;
        Self::add_traffic(&mut tx, Resolution::Hour, hours).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Adds `buckets` to the `resolution` table on `conn`: reads the rows stored for their
    /// starts, merges them in, and writes the sums back. A histogram cannot be merged in SQL.
    async fn add_traffic<L: Latencies>(
        conn: &mut SqliteConnection,
        resolution: Resolution,
        mut buckets: Buckets<L>,
    ) -> Result<()> {
        for start in buckets.starts() {
            for stored in Self::traffic_at(conn, resolution, start).await? {
                let stored = TrafficBucket::<L>::try_from(stored)?;
                if let Some(sum) = buckets.0.get_mut(&(stored.start, stored.series.clone())) {
                    *sum += stored;
                }
            }
        }
        for bucket in buckets.0.into_values() {
            Self::upsert_traffic(conn, resolution, bucket.into()).await?;
        }
        Ok(())
    }

    /// The rows of the `resolution` bucket that starts at `start`.
    async fn traffic_at(
        conn: &mut SqliteConnection,
        resolution: Resolution,
        start: Timestamp,
    ) -> Result<Vec<TrafficRow>> {
        let rows = match resolution {
            Resolution::Minute => {
                sqlx::query_as!(
                    TrafficRow,
                    r#"SELECT start AS "start: Timestamp", host, prefix, stack, service, none,
                              s1xx, s2xx, s3xx, s4xx, s5xx, upstream_errors, latency
                       FROM traffic_minutes WHERE start = ?"#,
                    start,
                )
                .fetch_all(conn)
                .await?
            }
            Resolution::Hour => {
                sqlx::query_as!(
                    TrafficRow,
                    r#"SELECT start AS "start: Timestamp", host, prefix, stack, service, none,
                              s1xx, s2xx, s3xx, s4xx, s5xx, upstream_errors, latency
                       FROM traffic_hours WHERE start = ?"#,
                    start,
                )
                .fetch_all(conn)
                .await?
            }
        };
        Ok(rows)
    }

    /// Writes `row` to the `resolution` table, replacing the counts and histogram of a row
    /// already there.
    async fn upsert_traffic(
        conn: &mut SqliteConnection,
        resolution: Resolution,
        row: TrafficRow,
    ) -> Result<()> {
        match resolution {
            Resolution::Minute => {
                sqlx::query!(
                    "INSERT INTO traffic_minutes (start, host, prefix, stack, service, none, s1xx,
                         s2xx, s3xx, s4xx, s5xx, upstream_errors, latency)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT (start, host, prefix, stack, service) DO UPDATE SET
                         none = excluded.none, s1xx = excluded.s1xx, s2xx = excluded.s2xx,
                         s3xx = excluded.s3xx, s4xx = excluded.s4xx, s5xx = excluded.s5xx,
                         upstream_errors = excluded.upstream_errors, latency = excluded.latency",
                    row.start,
                    row.host,
                    row.prefix,
                    row.stack,
                    row.service,
                    row.none,
                    row.s1xx,
                    row.s2xx,
                    row.s3xx,
                    row.s4xx,
                    row.s5xx,
                    row.upstream_errors,
                    row.latency,
                )
                .execute(conn)
                .await?;
            }
            Resolution::Hour => {
                sqlx::query!(
                    "INSERT INTO traffic_hours (start, host, prefix, stack, service, none, s1xx,
                         s2xx, s3xx, s4xx, s5xx, upstream_errors, latency)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT (start, host, prefix, stack, service) DO UPDATE SET
                         none = excluded.none, s1xx = excluded.s1xx, s2xx = excluded.s2xx,
                         s3xx = excluded.s3xx, s4xx = excluded.s4xx, s5xx = excluded.s5xx,
                         upstream_errors = excluded.upstream_errors, latency = excluded.latency",
                    row.start,
                    row.host,
                    row.prefix,
                    row.stack,
                    row.service,
                    row.none,
                    row.s1xx,
                    row.s2xx,
                    row.s3xx,
                    row.s4xx,
                    row.s5xx,
                    row.upstream_errors,
                    row.latency,
                )
                .execute(conn)
                .await?;
            }
        }
        Ok(())
    }

    /// The `resolution` buckets that start within `span`, of every series or only those of
    /// `stack`, ordered by start (then host, prefix, stack and service).
    pub async fn traffic<L: Latencies>(
        &self,
        resolution: Resolution,
        span: Range<Timestamp>,
        stack: Option<&str>,
    ) -> Result<Vec<TrafficBucket<L>>> {
        let Range {
            start: from,
            end: to,
        } = span;
        // One query per table and filter, so that a stack's span reads the stack index and
        // everything over a span reads the primary key.
        let rows = match (resolution, stack) {
            (Resolution::Minute, None) => {
                sqlx::query_as!(
                    TrafficRow,
                    r#"SELECT start AS "start: Timestamp", host, prefix, stack, service, none,
                              s1xx, s2xx, s3xx, s4xx, s5xx, upstream_errors, latency
                       FROM traffic_minutes WHERE start >= ? AND start < ?
                       ORDER BY start, host, prefix, stack, service"#,
                    from,
                    to,
                )
                .fetch_all(&self.pool)
                .await?
            }
            (Resolution::Minute, Some(stack)) => {
                sqlx::query_as!(
                    TrafficRow,
                    r#"SELECT start AS "start: Timestamp", host, prefix, stack, service, none,
                              s1xx, s2xx, s3xx, s4xx, s5xx, upstream_errors, latency
                       FROM traffic_minutes WHERE stack = ? AND start >= ? AND start < ?
                       ORDER BY start, host, prefix, stack, service"#,
                    stack,
                    from,
                    to,
                )
                .fetch_all(&self.pool)
                .await?
            }
            (Resolution::Hour, None) => {
                sqlx::query_as!(
                    TrafficRow,
                    r#"SELECT start AS "start: Timestamp", host, prefix, stack, service, none,
                              s1xx, s2xx, s3xx, s4xx, s5xx, upstream_errors, latency
                       FROM traffic_hours WHERE start >= ? AND start < ?
                       ORDER BY start, host, prefix, stack, service"#,
                    from,
                    to,
                )
                .fetch_all(&self.pool)
                .await?
            }
            (Resolution::Hour, Some(stack)) => {
                sqlx::query_as!(
                    TrafficRow,
                    r#"SELECT start AS "start: Timestamp", host, prefix, stack, service, none,
                              s1xx, s2xx, s3xx, s4xx, s5xx, upstream_errors, latency
                       FROM traffic_hours WHERE stack = ? AND start >= ? AND start < ?
                       ORDER BY start, host, prefix, stack, service"#,
                    stack,
                    from,
                    to,
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.into_iter().map(TrafficBucket::try_from).collect()
    }

    /// Deletes the `resolution` buckets that start before `cutoff` (the retention sweep); a
    /// bucket starting at `cutoff` stays. Returns how many rows.
    pub async fn prune_traffic(&self, resolution: Resolution, cutoff: Timestamp) -> Result<u64> {
        let done = match resolution {
            Resolution::Minute => {
                sqlx::query!("DELETE FROM traffic_minutes WHERE start < ?", cutoff)
                    .execute(&self.pool)
                    .await?
            }
            Resolution::Hour => {
                sqlx::query!("DELETE FROM traffic_hours WHERE start < ?", cutoff)
                    .execute(&self.pool)
                    .await?
            }
        };
        Ok(done.rows_affected())
    }
}
