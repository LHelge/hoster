//! The read side of `spans` (ADR-0069 §6): one trace's stored spans, for
//! [`LogStore::load_trace`]. The trace list does not read spans here: it merges request rows
//! and stored roots in SQL (`TraceQuery`, in `traces.rs`).

use std::time::Duration;

use sqlx::sqlite::SqliteRow;
use sqlx::{FromRow, QueryBuilder, Row as _, Sqlite};

use crate::store::Result;
use crate::store::logs::{LogStore, MicroTimestamp, Span, TraceId};

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
    /// The first `limit` stored spans of the trace `id`, by start (then span id). Empty for a
    /// trace that retention removed or no container exported; the proxy's own two spans are
    /// not here (ADR-0069 §9). Reads the read pool only.
    pub(super) async fn trace_spans_up_to(&self, id: TraceId, limit: usize) -> Result<Vec<Span>> {
        let mut sql = QueryBuilder::<Sqlite>::new(COLUMNS);
        sql.push(" WHERE trace_id = ")
            .push_bind(id.to_string())
            .push(" ORDER BY start, span_id LIMIT ")
            .push_bind(i64::try_from(limit).unwrap_or(i64::MAX));
        Ok(sql.build_query_as().fetch_all(&self.reads).await?)
    }
}
