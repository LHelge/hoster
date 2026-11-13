//! The read side of traces (ADR-0069 §6, §9) for the Telemetry page's Traces tab: the trace
//! list over a range, the root durations of its scatter, one trace for the waterfall, and the
//! log records made in a span.
//!
//! A trace is listed by its root. A request the proxy routed has its server span as the root,
//! built from the request row (which has no stored span); a trace an app started itself (a
//! background job) has a stored span without a parent. Both are read as one set: the request
//! rows that have span ids, and the stored roots of traces without such a row.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, QueryBuilder, Row as _, Sqlite};

use super::spans::decode;
use super::{EntryRow, LogEntry, LogQuery, LogScope};
use crate::store::logs::{LogStore, MicroTimestamp, Span, SpanId, Trace, TraceId, UnknownLogText};
use crate::store::{Result, StoreError, Timestamp};

/// Whether a trace ended in error: any of its spans did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TraceStatus {
    Ok,
    Error,
}

impl TraceStatus {
    pub const ALL: [TraceStatus; 2] = [TraceStatus::Ok, TraceStatus::Error];

    pub fn as_str(self) -> &'static str {
        match self {
            TraceStatus::Ok => "ok",
            TraceStatus::Error => "error",
        }
    }
}

impl From<bool> for TraceStatus {
    /// `true` for a trace in error.
    fn from(failed: bool) -> Self {
        if failed {
            TraceStatus::Error
        } else {
            TraceStatus::Ok
        }
    }
}

impl fmt::Display for TraceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TraceStatus {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| UnknownLogText::new("trace status", s))
    }
}

/// The order of the trace list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TraceSort {
    /// The latest start first.
    #[default]
    Newest,
    /// The longest root first.
    Slowest,
    /// Traces in error first, then the newest.
    ErrorsFirst,
}

impl TraceSort {
    pub const ALL: [TraceSort; 3] = [
        TraceSort::Newest,
        TraceSort::Slowest,
        TraceSort::ErrorsFirst,
    ];

    /// As a URL writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            TraceSort::Newest => "newest",
            TraceSort::Slowest => "slowest",
            TraceSort::ErrorsFirst => "errors",
        }
    }

    fn order_by(self) -> &'static str {
        match self {
            TraceSort::Newest => " ORDER BY start DESC, trace_id DESC",
            TraceSort::Slowest => " ORDER BY duration DESC, start DESC, trace_id DESC",
            TraceSort::ErrorsFirst => " ORDER BY failed DESC, start DESC, trace_id DESC",
        }
    }
}

impl fmt::Display for TraceSort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TraceSort {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|sort| sort.as_str() == s)
            .ok_or_else(|| UnknownLogText::new("trace sort", s))
    }
}

/// Which traces the trace list shows, and the scatter plots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceQuery {
    /// Traces whose root started at or after the start and before the end.
    pub range: Range<Timestamp>,
    /// [`LogScope::All`] or one stack; [`LogScope::Hoster`] matches nothing.
    pub scope: LogScope,
    /// Only traces with a span of one of these services, or a request routed to one of them;
    /// every trace when empty. [`Span::PROXY_SERVICE`] matches every proxied request.
    pub services: Vec<String>,
    pub status: Option<TraceStatus>,
    /// Only traces whose root took at least this long.
    pub min_duration: Option<Duration>,
    /// Grep: only traces with a span whose name contains this, ignoring ASCII case; the root's
    /// name is the proxy's server span's for a proxied request (OQ-43: no query language).
    pub grep: Option<String>,
    pub sort: TraceSort,
    /// The page, from 0, of [`TraceQuery::limit`] traces each.
    pub page: u32,
    /// Traces per page; clamped to `1..=`[`LogQuery::MAX_LIMIT`].
    pub limit: u32,
}

impl TraceQuery {
    /// Traces per page unless the caller asks otherwise.
    pub const DEFAULT_LIMIT: u32 = 50;
    /// The most points the scatter gets; more are sampled evenly by start.
    pub const MAX_POINTS: u32 = 1_000;

    /// The first page of the newest traces in `range` and `scope`, unfiltered.
    pub fn new(range: Range<Timestamp>, scope: LogScope) -> Self {
        Self {
            range,
            scope,
            services: Vec::new(),
            status: None,
            min_duration: None,
            grep: None,
            sort: TraceSort::default(),
            page: 0,
            limit: Self::DEFAULT_LIMIT,
        }
    }

    fn limit(&self) -> u32 {
        self.limit.clamp(1, LogQuery::MAX_LIMIT)
    }

    /// `WITH roots AS (…)`: one row per trace in the range and scope, from its request row or
    /// its stored root (the earliest, when a trace has several).
    fn push_roots(&self, sql: &mut QueryBuilder<Sqlite>) {
        sql.push(
            "WITH roots AS MATERIALIZED (\
             SELECT request_id AS trace_id, time * 1000 AS start, total_ms * 1000 AS duration, \
                    stack, service AS routed, method || ' ' || path AS name, \
                    (status >= 500 OR upstream_failed) AS failed, \
                    CASE WHEN client_span_id IS NULL THEN 1 ELSE 2 END AS proxy_spans \
             FROM requests",
        );
        LogQuery::push_where(sql, &self.range, &self.scope);
        sql.push(
            " AND server_span_id IS NOT NULL \
             UNION ALL \
             SELECT trace_id, min(start), duration, stack, NULL, name, status = 'error', 0 \
             FROM spans AS s WHERE parent_span_id IS NULL AND start >= ",
        )
        .push_bind(MicroTimestamp::from(self.range.start))
        .push(" AND start < ")
        .push_bind(MicroTimestamp::from(self.range.end));
        self.scope.push_sql(sql);
        sql.push(
            " AND NOT EXISTS (SELECT 1 FROM requests AS r WHERE r.request_id = s.trace_id \
                              AND r.server_span_id IS NOT NULL) \
             GROUP BY trace_id)",
        );
    }

    /// `WITH roots …, traces AS (…)`: the roots with their stored spans' count, services and
    /// status, then `SELECT <columns> FROM traces WHERE <the filters>`.
    fn push_traces(&self, sql: &mut QueryBuilder<Sqlite>, columns: &str) {
        self.push_roots(sql);
        sql.push(
            ", agg AS (\
             SELECT trace_id, count(*) AS n, max(status = 'error') AS failed, \
                    json_group_array(DISTINCT service) AS services \
             FROM spans WHERE trace_id IN (SELECT trace_id FROM roots) GROUP BY trace_id), \
             traces AS (\
             SELECT r.trace_id, r.start, r.duration, r.stack, r.routed, r.name, \
                    (r.failed OR coalesce(a.failed, 0)) AS failed, r.proxy_spans, \
                    coalesce(a.n, 0) AS stored, coalesce(a.services, '[]') AS services \
             FROM roots AS r LEFT JOIN agg AS a ON a.trace_id = r.trace_id) \
             SELECT ",
        )
        .push(columns)
        .push(" FROM traces WHERE 1");
        if let Some(status) = self.status {
            sql.push(match status {
                TraceStatus::Ok => " AND NOT failed",
                TraceStatus::Error => " AND failed",
            });
        }
        if let Some(min) = self.min_duration {
            sql.push(" AND duration >= ")
                .push_bind(LogStore::micros(min));
        }
        if let Some(grep) = self.grep.as_deref().filter(|g| !g.is_empty()) {
            let needle = grep.to_ascii_lowercase();
            sql.push(" AND (instr(lower(name), ")
                .push_bind(needle.clone())
                .push(
                    ") > 0 OR EXISTS (SELECT 1 FROM spans AS g \
                     WHERE g.trace_id = traces.trace_id AND instr(lower(g.name), ",
                )
                .push_bind(needle)
                .push(") > 0))");
        }
        if !self.services.is_empty() {
            sql.push(" AND (routed");
            self.push_services(sql);
            sql.push(" OR EXISTS (SELECT 1 FROM json_each(services) WHERE value");
            self.push_services(sql);
            sql.push(")");
            if self.services.iter().any(|s| s == Span::PROXY_SERVICE) {
                sql.push(" OR proxy_spans > 0");
            }
            sql.push(")");
        }
    }

    /// ` IN (<the services>)`.
    fn push_services(&self, sql: &mut QueryBuilder<Sqlite>) {
        sql.push(" IN (");
        let mut list = sql.separated(", ");
        for service in &self.services {
            list.push_bind(service.clone());
        }
        sql.push(")");
    }
}

/// One row of the trace list: a trace by its root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceSummary {
    pub trace_id: TraceId,
    /// When the root started.
    pub start: MicroTimestamp,
    /// The root span's name.
    pub name: String,
    pub stack: String,
    /// The services of its spans, each once: [`Span::PROXY_SERVICE`] first for a proxied
    /// request, then the others by name.
    pub services: Vec<String>,
    /// Its spans, the proxy's included.
    pub spans: u64,
    /// The root's duration.
    pub duration: Duration,
    /// [`TraceStatus::Error`] when any span is in error.
    pub status: TraceStatus,
    /// A proxied request whose app sent no spans: "app not instrumented".
    pub proxy_only: bool,
}

const SUMMARY_COLUMNS: &str = "trace_id, start, duration, stack, name, failed, proxy_spans, stored, services, \
     count(*) OVER () AS matched";

/// A [`TraceSummary`] with the number of traces its query matched.
struct SummaryRow {
    summary: TraceSummary,
    matched: i64,
}

fn micros(value: i64) -> Duration {
    Duration::from_micros(u64::try_from(value).unwrap_or(0))
}

impl<'r> FromRow<'r, SqliteRow> for SummaryRow {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        let proxy_spans: i64 = row.try_get("proxy_spans")?;
        let stored: i64 = row.try_get("stored")?;
        let mut services: Vec<String> =
            decode(serde_json::from_str(row.try_get::<&str, _>("services")?))?;
        services.sort();
        services.dedup();
        if proxy_spans > 0 {
            services.retain(|s| s != Span::PROXY_SERVICE);
            services.insert(0, Span::PROXY_SERVICE.to_owned());
        }
        Ok(Self {
            summary: TraceSummary {
                trace_id: decode(row.try_get::<String, _>("trace_id")?.parse())?,
                start: row.try_get("start")?,
                name: row.try_get("name")?,
                stack: row.try_get("stack")?,
                services,
                spans: u64::try_from(proxy_spans + stored).unwrap_or(0),
                duration: micros(row.try_get("duration")?),
                status: TraceStatus::from(row.try_get::<bool, _>("failed")?),
                proxy_only: proxy_spans > 0 && stored == 0,
            },
            matched: row.try_get("matched")?,
        })
    }
}

/// One page of the trace list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TracePage {
    /// In the query's order.
    pub traces: Vec<TraceSummary>,
    /// How many traces the query's filters match in all pages.
    pub matched: u64,
    /// How many traces the range and scope hold, unfiltered: the tab's count.
    pub total: u64,
    /// More matching traces follow this page.
    pub more: bool,
}

/// One point of the duration scatter: a trace's root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TracePoint {
    pub trace_id: TraceId,
    /// The root span's name, for the scatter's label of its slowest error.
    pub name: String,
    pub start: MicroTimestamp,
    pub duration: Duration,
    pub status: TraceStatus,
}

/// The duration scatter's points, by start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TracePoints {
    /// At most [`TraceQuery::MAX_POINTS`], sampled evenly by start when more match.
    pub points: Vec<TracePoint>,
    /// How many traces matched, sampled or not.
    pub matched: u64,
}

impl TracePoints {
    /// Whether the points are a sample.
    pub fn is_sampled(&self) -> bool {
        self.matched > self.points.len() as u64
    }
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn decode_error(e: UnknownLogText) -> StoreError {
    StoreError::Database(sqlx::Error::Decode(Box::new(e)))
}

impl LogStore {
    /// One page of the trace list `query` asks for, with how many traces it matches and how
    /// many the range holds. Reads the read pool only.
    pub async fn traces(&self, query: &TraceQuery) -> Result<TracePage> {
        let limit = query.limit();
        let mut sql = QueryBuilder::<Sqlite>::new("");
        query.push_traces(&mut sql, SUMMARY_COLUMNS);
        sql.push(query.sort.order_by())
            .push(" LIMIT ")
            .push_bind(i64::from(limit))
            .push(" OFFSET ")
            .push_bind(i64::from(limit) * i64::from(query.page));
        let rows: Vec<SummaryRow> = sql.build_query_as().fetch_all(&self.reads).await?;
        let matched = match rows.first() {
            Some(row) => count(row.matched),
            None if query.page == 0 => 0,
            None => {
                let mut sql = QueryBuilder::<Sqlite>::new("");
                query.push_traces(&mut sql, "count(*)");
                count(sql.build_query_scalar().fetch_one(&self.reads).await?)
            }
        };
        let total = self.count_traces(query.range.clone(), &query.scope).await?;
        let shown = u64::from(limit) * u64::from(query.page) + rows.len() as u64;
        Ok(TracePage {
            traces: rows.into_iter().map(|row| row.summary).collect(),
            matched,
            total,
            more: shown < matched,
        })
    }

    /// How many traces `range` and `scope` hold, unfiltered: [`TracePage::total`] without
    /// reading a page, for the Telemetry page's tab count on another tab.
    pub async fn count_traces(&self, range: Range<Timestamp>, scope: &LogScope) -> Result<u64> {
        let mut sql = QueryBuilder::<Sqlite>::new("");
        TraceQuery::new(range, scope.clone()).push_roots(&mut sql);
        sql.push(" SELECT count(*) FROM roots");
        Ok(count(
            sql.build_query_scalar().fetch_one(&self.reads).await?,
        ))
    }

    /// The roots of the traces `query` matches (its sort and page aside) for the duration
    /// scatter, by start; at most [`TraceQuery::MAX_POINTS`], picked evenly by start when more
    /// match.
    pub async fn trace_points(&self, query: &TraceQuery) -> Result<TracePoints> {
        let cap = i64::from(TraceQuery::MAX_POINTS);
        let mut sql =
            QueryBuilder::<Sqlite>::new("SELECT trace_id, name, start, duration, failed, n FROM (");
        query.push_traces(
            &mut sql,
            "trace_id, name, start, duration, failed, \
             row_number() OVER (ORDER BY start, trace_id) - 1 AS i, count(*) OVER () AS n",
        );
        // Of n rows, exactly `cap` have (i * cap) mod n below cap: one per stretch of n / cap.
        sql.push(") WHERE n <= ")
            .push_bind(cap)
            .push(" OR (i * ")
            .push_bind(cap)
            .push(") % n < ")
            .push_bind(cap)
            .push(" ORDER BY start, trace_id");
        let rows: Vec<(String, String, MicroTimestamp, i64, bool, i64)> =
            sql.build_query_as().fetch_all(&self.reads).await?;
        let matched = rows.first().map_or(0, |row| count(row.5));
        let points = rows
            .into_iter()
            .map(|(trace_id, name, start, duration, failed, _)| {
                Ok(TracePoint {
                    trace_id: trace_id.parse().map_err(decode_error)?,
                    name,
                    start,
                    duration: micros(duration),
                    status: TraceStatus::from(failed),
                })
            })
            .collect::<Result<_>>()?;
        Ok(TracePoints { points, matched })
    }

    /// The trace `id` for the waterfall: its stored spans and the proxy's two built from its
    /// request row (ADR-0069 §9), the earliest [`Trace::MAX_SPANS`] by start, with the log
    /// records made in each. `None` when neither a span nor a request row with span ids is
    /// left. Reads the read pool only.
    pub async fn load_trace(&self, id: TraceId) -> Result<Option<Trace>> {
        let mut spans = match self.request(id).await? {
            Some(entry) => entry.request.proxy_spans(),
            None => Vec::new(),
        };
        let proxy = spans.len() as u64;
        let mut sql = QueryBuilder::<Sqlite>::new("SELECT count(*) FROM spans WHERE trace_id = ");
        sql.push_bind(id.to_string());
        let stored = count(sql.build_query_scalar().fetch_one(&self.reads).await?);
        if proxy + stored == 0 {
            return Ok(None);
        }
        let room = Trace::MAX_SPANS.saturating_sub(spans.len());
        let mut sql = QueryBuilder::<Sqlite>::new(super::spans::COLUMNS);
        sql.push(" WHERE trace_id = ")
            .push_bind(id.to_string())
            .push(" ORDER BY start, span_id LIMIT ")
            .push_bind(i64::try_from(room).unwrap_or(i64::MAX));
        spans.extend(sql.build_query_as::<Span>().fetch_all(&self.reads).await?);
        let mut trace = Trace::new(id, spans, proxy + stored);

        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT span_id, count(*) FROM log_records WHERE trace_id = ",
        );
        sql.push_bind(id.to_string()).push(" GROUP BY span_id");
        let rows: Vec<(Option<String>, i64)> = sql.build_query_as().fetch_all(&self.reads).await?;
        let counts = rows
            .into_iter()
            .map(|(span, n)| {
                let span = span.as_deref().map(str::parse::<SpanId>).transpose();
                Ok((span.map_err(decode_error)?, count(n)))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        trace.count_log_records(counts);
        Ok(Some(trace))
    }

    /// The log records made in span `span` of trace `trace`, the oldest first, at most `limit`
    /// (clamped to `1..=`[`LogQuery::MAX_LIMIT`]): the span details' records.
    pub async fn span_records(
        &self,
        trace: TraceId,
        span: SpanId,
        limit: u32,
    ) -> Result<Vec<LogEntry>> {
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT id, time, stack, service, generation, stream, level, message, fields, cut, \
             marker, trace_id, span_id, received FROM log_records WHERE trace_id = ",
        );
        sql.push_bind(trace.to_string())
            .push(" AND span_id = ")
            .push_bind(span.to_string())
            .push(" ORDER BY time, id LIMIT ")
            .push_bind(i64::from(limit.clamp(1, LogQuery::MAX_LIMIT)));
        let rows: Vec<EntryRow> = sql.build_query_as().fetch_all(&self.reads).await?;
        rows.into_iter()
            .map(|row| LogEntry::try_from(row).map_err(decode_error))
            .collect()
    }
}
