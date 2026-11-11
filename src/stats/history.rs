//! Reading traffic history (ADR-0062 §8): what a page draws for a [`Range`], per scope.
//!
//! [`TrafficHistory`] is everything one render needs: for all hosted traffic, each stack,
//! hoster's own routes and the unrouted requests, a series of bars (the range's
//! [bar plan](Range::bars)), the totals over the range and over the period of equal length just
//! before it, the current rate and, for ranges drawn from minute buckets, the peak minute.
//! Pages never see rows: [`Stats::history`](super::Stats::history) reads them from the
//! [`StatsSource`](super::StatsSource) in one or two reads and hands this over.
//!
//! Only flushed data is read: the bars end with the newest minute the recorder has written (or,
//! for ranges drawn from hour buckets, with the hour that holds it, partial). They are rolling
//! windows counted back from there, not calendar days, so no time zone is involved.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::AddAssign;
use std::str::FromStr;
use std::time::Duration;

use super::{Histogram, StatusClass, StatusCounts};
use crate::store::{Resolution, Timestamp, TrafficBucket, TrafficSeries};

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(60 * 60);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// A time range the history is read over: the dashboard's picker (`?range=1h` … `?range=30d`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Range {
    /// `1h`: 60 bars of 1 min.
    Hour,
    /// `6h`: 72 bars of 5 min.
    SixHours,
    /// `24h`: 24 bars of 1 h.
    #[default]
    Day,
    /// `7d`: 56 bars of 3 h.
    Week,
    /// `30d`: 30 bars of 1 d.
    ThirtyDays,
}

impl Range {
    /// Every range, shortest first: the picker's order.
    pub const ALL: [Range; 5] = [
        Range::Hour,
        Range::SixHours,
        Range::Day,
        Range::Week,
        Range::ThirtyDays,
    ];

    /// How far back the range reaches.
    pub const fn length(self) -> Duration {
        match self {
            Range::Hour => HOUR,
            Range::SixHours => Duration::from_secs(6 * 60 * 60),
            Range::Day => DAY,
            Range::Week => Duration::from_secs(7 * 24 * 60 * 60),
            Range::ThirtyDays => Duration::from_secs(30 * 24 * 60 * 60),
        }
    }

    /// How many bars the range is drawn with (ADR-0062 §8).
    pub const fn bars(self) -> usize {
        match self {
            Range::Hour => 60,
            Range::SixHours => 72,
            Range::Day => 24,
            Range::Week => 56,
            Range::ThirtyDays => 30,
        }
    }

    /// How long one bar is.
    pub const fn bar(self) -> Duration {
        match self {
            Range::Hour => MINUTE,
            Range::SixHours => Duration::from_secs(5 * 60),
            Range::Day => HOUR,
            Range::Week => Duration::from_secs(3 * 60 * 60),
            Range::ThirtyDays => DAY,
        }
    }

    /// The table the range is read from: minute buckets up to 6 h, hour buckets beyond.
    pub const fn resolution(self) -> Resolution {
        match self {
            Range::Hour | Range::SixHours => Resolution::Minute,
            Range::Day | Range::Week | Range::ThirtyDays => Resolution::Hour,
        }
    }
}

/// The `?range=` value: `1h`, `6h`, `24h`, `7d` or `30d`.
impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Range::Hour => "1h",
            Range::SixHours => "6h",
            Range::Day => "24h",
            Range::Week => "7d",
            Range::ThirtyDays => "30d",
        })
    }
}

impl FromStr for Range {
    type Err = UnknownRange;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Range::ALL
            .into_iter()
            .find(|range| range.to_string() == s)
            .ok_or_else(|| UnknownRange(s.to_owned()))
    }
}

/// A `?range=` value that is none of [`Range::ALL`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown traffic range {0:?}; expected 1h, 6h, 24h, 7d or 30d")]
pub struct UnknownRange(String);

/// What a set of requests adds up to: over a bar, a range or the period before it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrafficTotals {
    /// Finished requests by status class.
    pub statuses: StatusCounts,
    /// Requests that failed because of their upstream.
    pub upstream_errors: u64,
    /// The latencies of the finished requests, merged.
    pub latency: Histogram,
}

#[allow(
    dead_code,
    reason = "read by the dashboard (h6k) and the stack pages (5q8), ADR-0062"
)]
impl TrafficTotals {
    /// Every finished request.
    pub fn requests(&self) -> u64 {
        self.statuses.total()
    }

    /// The 4xx responses.
    pub fn client_errors(&self) -> u64 {
        self.statuses.get(StatusClass::ClientError)
    }

    /// The 5xx responses.
    pub fn server_errors(&self) -> u64 {
        self.statuses.get(StatusClass::ServerError)
    }

    /// The share of 4xx responses, from 0 to 1; `None` without requests.
    pub fn client_error_share(&self) -> Option<f64> {
        self.share(self.client_errors())
    }

    /// The share of 5xx responses, from 0 to 1; `None` without requests.
    pub fn server_error_share(&self) -> Option<f64> {
        self.share(self.server_errors())
    }

    /// The median latency, to within 25 %; `None` without requests.
    pub fn p50(&self) -> Option<Duration> {
        self.latency.p50()
    }

    /// The 95th percentile latency, to within 25 %; `None` without requests.
    pub fn p95(&self) -> Option<Duration> {
        self.latency.p95()
    }

    fn share(&self, part: u64) -> Option<f64> {
        let requests = self.requests();
        (requests > 0).then(|| part as f64 / requests as f64)
    }
}

impl AddAssign<&TrafficBucket<Histogram>> for TrafficTotals {
    fn add_assign(&mut self, bucket: &TrafficBucket<Histogram>) {
        self.statuses += StatusCounts::from(&bucket.counts);
        self.upstream_errors = self
            .upstream_errors
            .saturating_add(bucket.counts.upstream_errors);
        self.latency += &bucket.latency;
    }
}

/// One bar of a series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    /// When the bar starts; it is [`Range::bar`] long.
    pub start: Timestamp,
    pub traffic: TrafficTotals,
}

/// The busiest minute of a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeakMinute {
    /// When the minute starts.
    pub at: Timestamp,
    /// The requests finished in it.
    pub requests: u64,
}

/// The history of one scope (all hosted traffic, a stack, …) over a range.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeHistory {
    /// Exactly [`Range::bars`] bars, oldest first; a bar without requests is empty.
    pub bars: Vec<Bar>,
    /// Over the whole range.
    pub totals: TrafficTotals,
    /// Over the period of equal length just before the range; `None` when it holds no data,
    /// since a change from nothing means nothing.
    pub previous: Option<TrafficTotals>,
    /// The requests of the last [`ScopeHistory::RATE_WINDOW`] of flushed minutes, whatever
    /// the range.
    pub recent: u64,
    /// The busiest minute of the range, for ranges drawn from minute buckets; `None` for the
    /// others, or without requests.
    pub peak: Option<PeakMinute>,
}

#[allow(
    dead_code,
    reason = "read by the dashboard (h6k) and the stack pages (5q8), ADR-0062"
)]
impl ScopeHistory {
    /// How far back the current rate looks (ADR-0062 §8).
    pub const RATE_WINDOW: Duration = Duration::from_secs(5 * 60);

    /// The current rate: requests per minute over the last [`Self::RATE_WINDOW`].
    pub fn per_minute(&self) -> f64 {
        self.recent as f64 / (Self::RATE_WINDOW.as_secs_f64() / 60.0)
    }

    /// No requests in the range, the period before it or the rate window: empty bars.
    fn empty(plan: &Plan) -> Self {
        ScopeHistory {
            bars: (0..plan.range.bars())
                .map(|i| Bar {
                    start: plan.start + plan.range.bar() * i as u32,
                    traffic: TrafficTotals::default(),
                })
                .collect(),
            totals: TrafficTotals::default(),
            previous: None,
            recent: 0,
            peak: None,
        }
    }
}

/// Where a range's bars lie, as of one moment.
#[derive(Debug, Clone, Copy)]
struct Plan {
    range: Range,
    /// The start of the first bar.
    start: Timestamp,
    /// The end of the last bar.
    end: Timestamp,
    /// The end of the newest flushed minute: the end of the rate window.
    flushed: Timestamp,
}

impl Plan {
    /// How long after a minute ends its flush can be counted on to be written: the recorder
    /// ticks a second after the minute, and a write takes a moment. Before that, the minute
    /// before is the newest flushed one.
    const FLUSH_LAG: Duration = Duration::from_secs(5);

    fn new(range: Range, now: Timestamp) -> Self {
        let flushed = Resolution::Minute.bucket(now - Self::FLUSH_LAG);
        let end = match range.resolution() {
            Resolution::Minute => flushed,
            // The hour that holds the newest flushed minute, partial.
            Resolution::Hour => Resolution::Hour.bucket(flushed - MINUTE) + HOUR,
        };
        Plan {
            range,
            start: end - range.length(),
            end,
            flushed,
        }
    }

    /// The start of the period before the range, which ends where the range starts.
    fn previous(&self) -> Timestamp {
        self.start - self.range.length()
    }

    /// The span of the rate window.
    fn recent(&self) -> std::ops::Range<Timestamp> {
        self.flushed - ScopeHistory::RATE_WINDOW..self.flushed
    }

    /// The index of the bar that holds `at`, if any.
    fn bar(&self, at: Timestamp) -> Option<usize> {
        if at < self.start || at >= self.end {
            return None;
        }
        let bar = i64::try_from(self.range.bar().as_millis()).unwrap_or(i64::MAX);
        usize::try_from((at.as_millis() - self.start.as_millis()) / bar).ok()
    }
}

/// A [`ScopeHistory`] being added up from rows.
struct ScopeBuilder {
    history: ScopeHistory,
    previous: Option<TrafficTotals>,
    /// Requests per minute bucket of the range, for the peak.
    minutes: BTreeMap<Timestamp, u64>,
}

impl ScopeBuilder {
    fn new(plan: &Plan) -> Self {
        ScopeBuilder {
            history: ScopeHistory::empty(plan),
            previous: None,
            minutes: BTreeMap::new(),
        }
    }

    /// Adds a row of the range's table to its bar and the totals, or to the previous period.
    fn add(&mut self, plan: &Plan, row: &TrafficBucket<Histogram>) {
        if let Some(bar) = plan.bar(row.start) {
            self.history.bars[bar].traffic += row;
            self.history.totals += row;
            if plan.range.resolution() == Resolution::Minute {
                *self.minutes.entry(row.start).or_default() +=
                    StatusCounts::from(&row.counts).total();
            }
        } else if row.start >= plan.previous() && row.start < plan.start {
            *self.previous.get_or_insert_default() += row;
        }
    }

    /// Adds a minute row of the rate window.
    fn add_recent(&mut self, row: &TrafficBucket<Histogram>) {
        self.history.recent = self
            .history
            .recent
            .saturating_add(StatusCounts::from(&row.counts).total());
    }

    fn build(self) -> ScopeHistory {
        // The latest of equally busy minutes: the one a reader most likely remembers.
        let peak = self
            .minutes
            .into_iter()
            .filter(|(_, requests)| *requests > 0)
            .max_by_key(|(at, requests)| (*requests, *at))
            .map(|(at, requests)| PeakMinute { at, requests });
        ScopeHistory {
            previous: self.previous,
            peak,
            ..self.history
        }
    }
}

/// Which scopes a row counts in besides "all": its stack, hoster's own purpose, or unrouted
/// (ADR-0062 §4).
enum Owner<'a> {
    Stack(&'a str),
    /// Hoster's own route (the admin UI): no stack, the purpose as the service.
    Reserved(&'a str),
    Unrouted,
}

impl<'a> From<&'a TrafficSeries> for Owner<'a> {
    fn from(series: &'a TrafficSeries) -> Self {
        match (series.stack.as_str(), series.service.as_str()) {
            ("", "") => Owner::Unrouted,
            ("", purpose) => Owner::Reserved(purpose),
            (stack, _) => Owner::Stack(stack),
        }
    }
}

/// The traffic history over one range, per scope: everything a page draws for it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrafficHistory {
    range: Range,
    end: Timestamp,
    all: ScopeHistory,
    stacks: BTreeMap<String, ScopeHistory>,
    reserved: BTreeMap<String, ScopeHistory>,
    unrouted: ScopeHistory,
    /// What a scope without rows shows.
    empty: ScopeHistory,
}

#[allow(
    dead_code,
    reason = "read by the dashboard (h6k) and the stack pages (5q8), ADR-0062"
)]
impl TrafficHistory {
    /// The history over `range` as of `now`, from `rows` (the buckets of the range's table
    /// from the start of the previous period to the end of the range) and `recent` (the minute
    /// buckets of the rate window, when the range is not drawn from minute buckets; those of
    /// `rows` otherwise). Rows outside their span are ignored.
    pub(super) fn new(
        range: Range,
        now: Timestamp,
        rows: &[TrafficBucket<Histogram>],
        recent: Option<&[TrafficBucket<Histogram>]>,
    ) -> Self {
        let plan = Plan::new(range, now);
        let mut all = ScopeBuilder::new(&plan);
        let mut stacks: BTreeMap<String, ScopeBuilder> = BTreeMap::new();
        let mut reserved: BTreeMap<String, ScopeBuilder> = BTreeMap::new();
        let mut unrouted = ScopeBuilder::new(&plan);
        let mut scopes =
            |row: &TrafficBucket<Histogram>, add: &dyn Fn(&mut ScopeBuilder)| match Owner::from(
                &row.series,
            ) {
                Owner::Stack(stack) => {
                    add(&mut all);
                    add(stacks
                        .entry(stack.to_owned())
                        .or_insert_with(|| ScopeBuilder::new(&plan)));
                }
                Owner::Reserved(purpose) => add(reserved
                    .entry(purpose.to_owned())
                    .or_insert_with(|| ScopeBuilder::new(&plan))),
                Owner::Unrouted => {
                    add(&mut all);
                    add(&mut unrouted);
                }
            };
        let in_window = plan.recent();
        for row in rows {
            scopes(row, &|scope| scope.add(&plan, row));
        }
        let recent = recent.unwrap_or(match range.resolution() {
            Resolution::Minute => rows,
            Resolution::Hour => &[],
        });
        for row in recent.iter().filter(|row| in_window.contains(&row.start)) {
            scopes(row, &|scope| scope.add_recent(row));
        }
        let build = |scopes: BTreeMap<String, ScopeBuilder>| {
            scopes
                .into_iter()
                .map(|(name, scope)| (name, scope.build()))
                .collect()
        };
        TrafficHistory {
            range,
            end: plan.end,
            all: all.build(),
            stacks: build(stacks),
            reserved: build(reserved),
            unrouted: unrouted.build(),
            empty: ScopeHistory::empty(&plan),
        }
    }

    /// The spans [`Self::new`] reads, as of `now`: the range's table from the start of the
    /// previous period to the end of the range, and the rate window's minutes when that table
    /// is not the minute one.
    pub(super) fn spans(
        range: Range,
        now: Timestamp,
    ) -> (
        std::ops::Range<Timestamp>,
        Option<std::ops::Range<Timestamp>>,
    ) {
        let plan = Plan::new(range, now);
        let rows = plan.previous()..plan.end;
        let recent = (range.resolution() == Resolution::Hour).then(|| plan.recent());
        (rows, recent)
    }

    /// The range read.
    pub fn range(&self) -> Range {
        self.range
    }

    /// The end of the last bar: the end of the newest flushed minute, or of the hour that
    /// holds it for ranges drawn from hour buckets.
    pub fn end(&self) -> Timestamp {
        self.end
    }

    /// All hosted traffic: every stack's routes (removed stacks' too) and the unrouted
    /// requests, without hoster's own routes.
    pub fn all(&self) -> &ScopeHistory {
        &self.all
    }

    /// The traffic of `stack`'s routes; empty bars when it saw none.
    pub fn stack(&self, stack: &str) -> &ScopeHistory {
        self.stacks.get(stack).unwrap_or(&self.empty)
    }

    /// Every stack that saw traffic in the range, the period before it or the rate window,
    /// removed stacks included, sorted by name.
    pub fn stacks(&self) -> impl Iterator<Item = (&str, &ScopeHistory)> {
        self.stacks
            .iter()
            .map(|(name, scope)| (name.as_str(), scope))
    }

    /// Hoster's own routes that saw traffic, by purpose (`admin UI`), sorted.
    pub fn reserved(&self) -> impl Iterator<Item = (&str, &ScopeHistory)> {
        self.reserved
            .iter()
            .map(|(purpose, scope)| (purpose.as_str(), scope))
    }

    /// The requests that matched no route.
    pub fn unrouted(&self) -> &ScopeHistory {
        &self.unrouted
    }
}

#[cfg(test)]
mod tests;
