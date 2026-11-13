//! The read side of `spans` (ADR-0069 §6): one trace's spans, and the root spans over a range
//! that the trace list starts from.

use std::ops::Range;
use std::time::Duration;

use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, QueryBuilder, Row as _, Sqlite};

use super::{LogQuery, LogScope};
use crate::store::logs::{LogStore, MicroTimestamp, Span, TraceId};
use crate::store::{Result, Timestamp};

/// Which root spans, the spans without a parent, a read returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootQuery {
    /// Roots that started at or after the start and before the end.
    pub range: Range<Timestamp>,
    /// [`LogScope::All`] or one stack; [`LogScope::Hoster`] matches nothing, as hoster
    /// exports no spans.
    pub scope: LogScope,
    /// At most this many, the newest first; clamped to `1..=`[`LogQuery::MAX_LIMIT`].
    pub limit: u32,
}

impl RootQuery {
    /// The newest roots in `range` and `scope`.
    pub fn new(range: Range<Timestamp>, scope: LogScope) -> Self {
        Self {
            range,
            scope,
            limit: LogQuery::DEFAULT_LIMIT,
        }
    }
}

const COLUMNS: &str = "SELECT trace_id, span_id, parent_span_id, stack, service, name, kind, \
                       start, duration, status, status_message, attributes, resource, events, \
                       received FROM spans";

/// Text that does not decode, as sqlx reports a column it cannot read.
fn decode<T, E>(parsed: std::result::Result<T, E>) -> sqlx::Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    parsed.map_err(|e| sqlx::Error::Decode(Box::new(e)))
}

impl<'r> FromRow<'r, SqliteRow> for Span {
    fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
        let parent: Option<String> = row.try_get("parent_span_id")?;
        let duration: i64 = row.try_get("duration")?;
        Ok(Self {
            trace_id: decode(row.try_get::<String, _>("trace_id")?.parse())?,
            span_id: decode(row.try_get::<String, _>("span_id")?.parse())?,
            parent: decode(parent.as_deref().map(str::parse).transpose())?,
            stack: row.try_get("stack")?,
            service: row.try_get("service")?,
            name: row.try_get("name")?,
            kind: decode(row.try_get::<String, _>("kind")?.parse())?,
            start: row.try_get::<MicroTimestamp, _>("start")?,
            duration: Duration::from_micros(u64::try_from(duration).unwrap_or(0)),
            status: decode(row.try_get::<String, _>("status")?.parse())?,
            status_message: row.try_get("status_message")?,
            attributes: row.try_get("attributes")?,
            resource: row.try_get("resource")?,
            events: row.try_get("events")?,
            received: row.try_get("received")?,
        })
    }
}

impl LogStore {
    /// Every stored span of the trace `id`, by start (then span id). Empty for a trace that
    /// retention removed or no container exported; the proxy's own two spans are not here
    /// (ADR-0069 §9). Reads the read pool only.
    pub async fn trace_spans(&self, id: TraceId) -> Result<Vec<Span>> {
        let mut sql = QueryBuilder::<Sqlite>::new(COLUMNS);
        sql.push(" WHERE trace_id = ")
            .push_bind(id.to_string())
            .push(" ORDER BY start, span_id");
        Ok(sql.build_query_as().fetch_all(&self.reads).await?)
    }

    /// The root spans `query` matches, the newest first, read with the partial index on spans
    /// without a parent. A trace whose root was lost, or whose root is the proxy's (built from
    /// its request row, ADR-0069 §9), has no root here.
    pub async fn root_spans(&self, query: &RootQuery) -> Result<Vec<Span>> {
        let mut sql = QueryBuilder::<Sqlite>::new(COLUMNS);
        sql.push(" WHERE parent_span_id IS NULL AND start >= ")
            .push_bind(MicroTimestamp::from(query.range.start))
            .push(" AND start < ")
            .push_bind(MicroTimestamp::from(query.range.end));
        query.scope.push_sql(&mut sql);
        sql.push(" ORDER BY start DESC, id DESC LIMIT ")
            .push_bind(i64::from(query.limit.clamp(1, LogQuery::MAX_LIMIT)));
        Ok(sql.build_query_as().fetch_all(&self.reads).await?)
    }
}
