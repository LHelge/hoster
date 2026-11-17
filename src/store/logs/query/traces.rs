//! The read side of the Traces tab (ADR-0069 §6, §9): the trace list over a range, the points
//! of its duration scatter, one trace for the waterfall, how many log records a trace has, and
//! the records of one of its spans.
//!
//! A trace is listed by its root. A request the proxy routed has its root built from the
//! request row (the proxy's server span, ADR-0069 §9), so it is listed whether or not the app
//! exported anything; a trace an app started itself (a background job) has its root in
//! `spans`. Both kinds are merged into one row per trace id in SQL: the request rows that have
//! span ids, and the stored spans without a parent, each read with its time index. A trace
//! whose root was lost (a dropped request row, a root not received) is not listed, though
//! [`LogStore::load_trace`] still reads it.
//!
//! The list's figures (span count, status, services) come from the trace's stored spans by the
//! `spans_by_trace` index, per listed root.

use std::fmt;
use std::ops::Range;
use std::str::FromStr;
use std::time::Duration;

use sqlx::{QueryBuilder, Sqlite};

use super::{EntryRow, LogEntry, LogQuery, LogScope};
use crate::store::logs::{LogStore, MicroTimestamp, Span, SpanId, Trace, TraceId, UnknownLogText};
use crate::store::{Result, StoreError, Timestamp};

/// A trace's outcome: in error when any of its spans is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TraceStatus {
    Ok,
    Error,
}

impl TraceStatus {
    /// Both, ok first.
    pub const ALL: [TraceStatus; 2] = [TraceStatus::Ok, TraceStatus::Error];

    /// As a URL writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            TraceStatus::Ok => "ok",
            TraceStatus::Error => "error",
        }
    }
}

impl From<bool> for TraceStatus {
    /// `true` for a trace in error.
    fn from(error: bool) -> Self {
        if error {
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
    /// The latest root start first.
    #[default]
    Newest,
    /// The longest root first, then the newest.
    Slowest,
    /// Traces in error first, then the newest.
    ErrorsFirst,
}

impl TraceSort {
    /// Every order, the default first.
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

    /// The `ORDER BY` terms, with the trace id last so equal rows keep one order across pages.
    fn order_by(self) -> &'static str {
        match self {
            TraceSort::Newest => " ORDER BY start DESC, trace_id DESC",
            TraceSort::Slowest => " ORDER BY duration DESC, start DESC, trace_id DESC",
            TraceSort::ErrorsFirst => " ORDER BY error DESC, start DESC, trace_id DESC",
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

/// Which traces the trace list shows, and in which order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceQuery {
    /// Traces whose root started at or after the start and before the end.
    pub range: Range<Timestamp>,
    /// [`LogScope::All`] or one stack (the root's); [`LogScope::Hoster`] matches nothing.
    pub scope: LogScope,
    /// Only traces with a span of one of these services ([`Span::EDGE_SERVICE`] for the
    /// proxy's); every trace when empty.
    pub services: Vec<String>,
    pub status: Option<TraceStatus>,
    /// Only traces whose root took at least this long.
    pub min_duration: Option<Duration>,
    /// Grep on span names: only traces whose root's name or a stored span's name contains this,
    /// ignoring ASCII case (the Traces tab's query box, OQ-43); every trace when empty.
    pub name: Option<String>,
    pub sort: TraceSort,
    /// Which page, from 0.
    pub page: u32,
    /// Traces per page; clamped to `1..=`[`LogQuery::MAX_LIMIT`].
    pub limit: u32,
}

impl TraceQuery {
    /// Traces per page unless the caller asks otherwise.
    pub const DEFAULT_LIMIT: u32 = 50;

    /// The first page of the newest traces in `range` and `scope`, unfiltered.
    pub fn new(range: Range<Timestamp>, scope: LogScope) -> Self {
        Self {
            range,
            scope,
            services: Vec::new(),
            status: None,
            min_duration: None,
            name: None,
            sort: TraceSort::Newest,
            page: 0,
            limit: Self::DEFAULT_LIMIT,
        }
    }

    fn limit(&self) -> u32 {
        self.limit.clamp(1, LogQuery::MAX_LIMIT)
    }

    /// `WITH roots AS (…), traces AS (…) `: one row per trace with a root in the range and
    /// scope, with its start and duration in µs, its root's stack and name, whether the root is
    /// the proxy's (`proxied`) and in error itself (`failed`), and the proxy's spans (`edge`).
    /// Of two roots of one trace the earlier counts. With `error`, also whether any of its
    /// spans is in error, which looks up the trace's stored spans for every root: only when a
    /// filter or the order needs it for all of them.
    fn push_traces(&self, sql: &mut QueryBuilder<Sqlite>, error: bool) {
        sql.push(
            "WITH roots AS ( \
               SELECT trace_id, min(start) AS start, duration, stack, name, proxied, failed, edge \
               FROM ( \
                 SELECT request_id AS trace_id, time * 1000 AS start, total_ms * 1000 AS duration, \
                        stack, method || ' ' || path AS name, 1 AS proxied, \
                        (status >= 500 OR upstream_failed) AS failed, \
                        1 + (client_span_id IS NOT NULL) AS edge \
                 FROM requests",
        );
        LogQuery::push_where(sql, &self.range, &self.scope);
        sql.push(
            " AND server_span_id IS NOT NULL \
             UNION ALL \
             SELECT trace_id, start, duration, stack, name, 0, status = 'error', 0 \
             FROM spans WHERE parent_span_id IS NULL AND start >= ",
        )
        .push_bind(MicroTimestamp::from(self.range.start))
        .push(" AND start < ")
        .push_bind(MicroTimestamp::from(self.range.end));
        self.scope.push_sql(sql);
        sql.push(
            ") GROUP BY trace_id \
             ), \
             traces AS ( \
               SELECT trace_id, start, duration, stack, name, proxied, failed, edge",
        );
        if error {
            sql.push(", ")
                .push(Self::error_of("roots"))
                .push(" AS error");
        }
        sql.push(" FROM roots) ");
    }

    /// Whether any span of the trace in row `table` is in error: its root, or a stored span.
    fn error_of(table: &str) -> String {
        format!(
            "({table}.failed OR EXISTS (SELECT 1 FROM spans s \
             WHERE s.trace_id = {table}.trace_id AND s.status = 'error'))"
        )
    }

    /// Whether [`TraceQuery::push_filters`] reads the `error` column.
    fn filters_need_error(&self) -> bool {
        self.status.is_some()
    }

    /// The filters as one SQL expression over `traces`: `1` without any. With a status, the
    /// `traces` CTE needs its `error` column.
    fn push_filters(&self, sql: &mut QueryBuilder<Sqlite>) {
        sql.push("(1");
        if let Some(status) = self.status {
            sql.push(" AND error = ")
                .push_bind(status == TraceStatus::Error);
        }
        if let Some(min) = self.min_duration {
            sql.push(" AND duration >= ")
                .push_bind(LogStore::micros(min));
        }
        if !self.services.is_empty() {
            let edge = self.services.iter().any(|s| s == Span::EDGE_SERVICE);
            sql.push(" AND (")
                .push(if edge { "proxied OR " } else { "" })
                .push("EXISTS (SELECT 1 FROM spans s WHERE s.trace_id = traces.trace_id")
                .push(" AND s.service IN (");
            let mut list = sql.separated(", ");
            for service in &self.services {
                list.push_bind(service.clone());
            }
            sql.push(")))");
        }
        if let Some(name) = self.name.as_deref().filter(|name| !name.is_empty()) {
            // `instr` over `lower`: no `LIKE` wildcards to escape. SQLite lowers ASCII only.
            let name = name.to_ascii_lowercase();
            sql.push(" AND (instr(lower(name), ")
                .push_bind(name.clone())
                .push(") > 0 OR EXISTS (SELECT 1 FROM spans s WHERE s.trace_id = traces.trace_id")
                .push(" AND instr(lower(s.name), ")
                .push_bind(name)
                .push(") > 0))");
        }
        sql.push(")");
    }

    /// The count of every trace in the range and scope, and of those the filters match.
    fn count_sql(&self) -> QueryBuilder<Sqlite> {
        let mut sql = QueryBuilder::<Sqlite>::new("");
        self.push_traces(&mut sql, self.filters_need_error());
        sql.push("SELECT count(*), coalesce(sum(");
        self.push_filters(&mut sql);
        sql.push("), 0) FROM traces");
        sql
    }

    /// The read of one page of the list, after `prefix` (`EXPLAIN QUERY PLAN ` in the tests).
    /// The page is chosen first, looking up stored spans for every root only where the filters
    /// or the order need it; its rows' span counts, status and services are then read for those
    /// rows alone.
    pub(in crate::store::logs) fn page_sql(&self, prefix: &str) -> QueryBuilder<Sqlite> {
        let mut sql = QueryBuilder::<Sqlite>::new(prefix);
        let error = self.filters_need_error() || self.sort == TraceSort::ErrorsFirst;
        self.push_traces(&mut sql, error);
        sql.push(
            "SELECT trace_id, start, duration, stack, name, proxied, edge, \
                    (SELECT count(*) FROM spans s WHERE s.trace_id = page.trace_id) AS stored, ",
        )
        .push(Self::error_of("page"))
        .push(
            " AS error, \
               (SELECT json_group_array(service) FROM ( \
                  SELECT service FROM spans s WHERE s.trace_id = page.trace_id \
                  GROUP BY service ORDER BY min(start), service)) \
             FROM (SELECT trace_id, start, duration, stack, name, proxied, failed, edge",
        );
        if error {
            sql.push(", error");
        }
        sql.push(" FROM traces WHERE ");
        self.push_filters(&mut sql);
        sql.push(self.sort.order_by());
        let limit = self.limit();
        sql.push(" LIMIT ")
            .push_bind(i64::from(limit))
            .push(" OFFSET ")
            .push_bind(i64::from(self.page) * i64::from(limit));
        // The page's own order, now over its few rows.
        sql.push(") AS page").push(self.sort.order_by());
        sql
    }

    /// The scatter's points: every matching trace, or [`TracePoints::CAP`] of them sampled
    /// evenly by start, with the status read for those points alone unless a filter needs it
    /// for every root.
    fn points_sql(&self) -> QueryBuilder<Sqlite> {
        let cap = i64::from(TracePoints::CAP);
        let mut sql = QueryBuilder::<Sqlite>::new("");
        self.push_traces(&mut sql, self.filters_need_error());
        sql.push("SELECT trace_id, start, duration, ")
            .push(Self::error_of("sample"))
            .push(
                ", matching FROM ( \
                   SELECT trace_id, start, duration, failed, \
                          row_number() OVER (ORDER BY start, trace_id) - 1 AS n, \
                          count(*) OVER () AS matching \
                   FROM traces WHERE ",
            );
        self.push_filters(&mut sql);
        // Row n is kept when it is the first of its share of the cap: exactly `cap` rows.
        sql.push(") AS sample WHERE (n * ")
            .push_bind(cap)
            .push(") % matching < ")
            .push_bind(cap)
            .push(" ORDER BY start, trace_id");
        sql
    }
}

/// One row of the trace list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceSummary {
    pub trace_id: TraceId,
    /// When its root started.
    pub start: MicroTimestamp,
    /// Its root's name: the method and path of a proxied request.
    pub name: String,
    /// Its root's stack.
    pub stack: String,
    /// Every service with a span in it, in the order their first spans started; the proxy's
    /// ([`Span::EDGE_SERVICE`]) first for a proxied request.
    pub services: Vec<String>,
    /// Every span it has, the proxy's included.
    pub spans: u64,
    /// Its root's duration.
    pub duration: Duration,
    pub status: TraceStatus,
    /// It has only the proxy's spans: the app is not instrumented.
    pub proxy_only: bool,
}

/// A page of the trace list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraceList {
    pub traces: Vec<TraceSummary>,
    /// The traces the query's filters match, over every page.
    pub matching: u64,
    /// Every trace in the query's range and scope, unfiltered: the tab's count.
    pub total: u64,
}

/// One trace in the duration scatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TracePoint {
    pub trace_id: TraceId,
    pub start: MicroTimestamp,
    pub duration: Duration,
    pub status: TraceStatus,
}

/// The points of the duration scatter: at most [`TracePoints::CAP`], sampled evenly by start
/// when more traces match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TracePoints {
    /// By start.
    pub points: Vec<TracePoint>,
    /// How many traces match: more than the points when they were sampled.
    pub matching: u64,
}

impl TracePoints {
    /// The most points one scatter draws.
    pub const CAP: u32 = 1000;
}

/// Text that does not decode, as sqlx reports a column it cannot read.
fn decode_err(e: UnknownLogText) -> StoreError {
    StoreError::Database(sqlx::Error::Decode(Box::new(e)))
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn micros(n: i64) -> Duration {
    Duration::from_micros(u64::try_from(n).unwrap_or(0))
}

type SummaryRow = (
    String,
    i64,
    i64,
    String,
    String,
    bool,
    i64,
    i64,
    bool,
    String,
);

impl LogStore {
    /// How many traces have a root in `range` and `scope`: the count [`LogStore::traces`] gives
    /// as [`TraceList::total`], for a page that lists none (the Telemetry page's Logs tab).
    pub async fn count_traces(&self, range: Range<Timestamp>, scope: LogScope) -> Result<u64> {
        let (total, _): (i64, i64) = TraceQuery::new(range, scope)
            .count_sql()
            .build_query_as()
            .fetch_one(&self.reads)
            .await?;
        Ok(count(total))
    }

    /// One page of the traces `query` matches, in its order, with how many match and how many
    /// the range and scope hold. Reads the read pool only.
    pub async fn traces(&self, query: &TraceQuery) -> Result<TraceList> {
        let (total, matching): (i64, i64) = query
            .count_sql()
            .build_query_as()
            .fetch_one(&self.reads)
            .await?;

        let rows: Vec<SummaryRow> = query
            .page_sql("")
            .build_query_as()
            .fetch_all(&self.reads)
            .await?;

        let mut traces = Vec::with_capacity(rows.len());
        for (trace_id, start, duration, stack, name, proxied, edge, stored, error, services) in rows
        {
            let stored_services: Vec<String> = serde_json::from_str(&services)
                .map_err(|e| StoreError::Database(sqlx::Error::Decode(Box::new(e))))?;
            let mut services = Vec::with_capacity(stored_services.len() + 1);
            if proxied {
                services.push(Span::EDGE_SERVICE.to_owned());
            }
            for service in stored_services {
                if !services.contains(&service) {
                    services.push(service);
                }
            }
            traces.push(TraceSummary {
                trace_id: trace_id.parse().map_err(decode_err)?,
                start: MicroTimestamp::from_micros(start),
                name,
                stack,
                services,
                spans: count(edge) + count(stored),
                duration: micros(duration),
                status: error.into(),
                proxy_only: proxied && stored == 0,
            });
        }
        Ok(TraceList {
            traces,
            matching: count(matching),
            total: count(total),
        })
    }

    /// The traces `query` matches as points of the duration scatter (its sort and page do not
    /// apply): every one up to [`TracePoints::CAP`], and that many sampled evenly in start
    /// order when there are more, so busy stretches keep their share.
    pub async fn trace_points(&self, query: &TraceQuery) -> Result<TracePoints> {
        let rows: Vec<(String, i64, i64, bool, i64)> = query
            .points_sql()
            .build_query_as()
            .fetch_all(&self.reads)
            .await?;

        let matching = rows.first().map_or(0, |row| count(row.4));
        let points = rows
            .into_iter()
            .map(|(trace_id, start, duration, error, _)| {
                Ok(TracePoint {
                    trace_id: trace_id.parse().map_err(decode_err)?,
                    start: MicroTimestamp::from_micros(start),
                    duration: micros(duration),
                    status: error.into(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(TracePoints { points, matching })
    }

    /// The trace `id` for the waterfall: the proxy's spans built from its request row, if it
    /// was a request with span ids (ADR-0069 §9), and its stored spans, the earliest first, up
    /// to [`Trace::SPAN_CAP`] in all, with the true count. `None` when it has neither.
    pub async fn load_trace(&self, id: TraceId) -> Result<Option<Trace>> {
        let edge = self
            .request(id)
            .await?
            .map(|entry| entry.request.edge_spans())
            .unwrap_or_default();
        let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM spans WHERE trace_id = ?")
            .bind(id.to_string())
            .fetch_one(&self.reads)
            .await?;
        let room = Trace::SPAN_CAP.saturating_sub(edge.len());
        let mut spans = if stored > 0 {
            self.trace_spans_up_to(id, room).await?
        } else {
            Vec::new()
        };
        if spans.is_empty() && edge.is_empty() {
            return Ok(None);
        }
        let total = count(stored) + u64::try_from(edge.len()).unwrap_or(0);
        spans.extend(edge);
        Ok(Some(Trace::new(id, spans, total)))
    }

    /// How many log records the trace `id` has, those without a span included: the
    /// waterfall's "logs in trace (N)". Counted with the trace index.
    pub async fn trace_record_count(&self, id: TraceId) -> Result<u64> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM log_records WHERE trace_id = ?")
            .bind(id.to_string())
            .fetch_one(&self.reads)
            .await?;
        Ok(count(n))
    }

    /// The log records of span `span` of trace `trace`, oldest first, at most `limit` (clamped
    /// to `1..=`[`LogQuery::MAX_LIMIT`]): the span's records in its details.
    pub async fn span_records(
        &self,
        trace: TraceId,
        span: SpanId,
        limit: u32,
    ) -> Result<Vec<LogEntry>> {
        let rows: Vec<EntryRow> = sqlx::query_as(
            "SELECT id, time, stack, service, generation, stream, level, message, fields, cut, \
             marker, trace_id, span_id, received FROM log_records \
             WHERE trace_id = ? AND span_id = ? ORDER BY time, id LIMIT ?",
        )
        .bind(trace.to_string())
        .bind(span.to_string())
        .bind(i64::from(limit.clamp(1, LogQuery::MAX_LIMIT)))
        .fetch_all(&self.reads)
        .await?;
        rows.into_iter()
            .map(|row| LogEntry::try_from(row).map_err(decode_err))
            .collect()
    }
}
