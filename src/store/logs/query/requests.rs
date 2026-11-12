//! The read side of `requests` (ADR-0067 §5): the access-log source of the stack logs tab and,
//! by request id, the row a container record links to.

use std::fmt;
use std::ops::Range;
use std::str::FromStr;
use std::time::Duration;

use sqlx::{QueryBuilder, Sqlite};

use super::{LogQuery, LogScope, Page, PageAt, Paged, RecordKey, TextFilter};
use crate::store::logs::{LogStore, RequestId, RequestRow, UnknownLogText};
use crate::store::{Result, StoreError, Timestamp};

/// Which statuses a request query keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    /// One class by its hundreds digit, 1 to 5: `4` is every 4xx. Written `4xx`.
    Class(u8),
    /// One code, e.g. `404`. `0` is a request whose response never went out.
    Code(u16),
}

impl StatusFilter {
    /// `AND <the status test>`.
    fn push_sql(self, sql: &mut QueryBuilder<Sqlite>) {
        match self {
            StatusFilter::Class(class) => {
                let low = i64::from(class) * 100;
                sql.push(" AND status >= ")
                    .push_bind(low)
                    .push(" AND status < ")
                    .push_bind(low + 100);
            }
            StatusFilter::Code(code) => {
                sql.push(" AND status = ").push_bind(i64::from(code));
            }
        }
    }
}

impl fmt::Display for StatusFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StatusFilter::Class(class) => write!(f, "{class}xx"),
            StatusFilter::Code(code) => write!(f, "{code}"),
        }
    }
}

/// `1xx` to `5xx` (the `x`s in any case), or a code of up to three digits.
impl FromStr for StatusFilter {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || UnknownLogText::new("status filter", s);
        let lower = s.to_ascii_lowercase();
        if let Some(digit) = lower.strip_suffix("xx") {
            return match digit.parse() {
                Ok(class @ 1..=5) if digit.len() == 1 => Ok(StatusFilter::Class(class)),
                _ => Err(invalid()),
            };
        }
        match s.parse() {
            Ok(code @ 0..=999) if s.bytes().all(|b| b.is_ascii_digit()) => {
                Ok(StatusFilter::Code(code))
            }
            _ => Err(invalid()),
        }
    }
}

/// Which request rows a page of the log store shows.
#[derive(Debug, Clone)]
pub struct RequestQuery {
    /// Requests that arrived at or after the start and before the end.
    pub range: Range<Timestamp>,
    /// [`LogScope::All`] or one stack; [`LogScope::Hoster`] matches nothing, as hoster's own
    /// hosts are not stored.
    pub scope: LogScope,
    /// Only these services; every service when empty.
    pub services: Vec<String>,
    pub status: Option<StatusFilter>,
    /// Only requests that took at least this long in total.
    pub min_total: Option<Duration>,
    /// What the path has to contain or match.
    pub path: Option<TextFilter>,
    /// Only the request with this id.
    pub request_id: Option<RequestId>,
    pub page: PageAt,
    /// At most this many rows; clamped to `1..=`[`LogQuery::MAX_LIMIT`].
    pub limit: u32,
}

impl RequestQuery {
    /// The newest page of every request in `range` and `scope`, unfiltered.
    pub fn new(range: Range<Timestamp>, scope: LogScope) -> Self {
        Self {
            range,
            scope,
            services: Vec::new(),
            status: None,
            min_total: None,
            path: None,
            request_id: None,
            page: PageAt::Newest,
            limit: LogQuery::DEFAULT_LIMIT,
        }
    }
}

const COLUMNS: &str = "SELECT id, time, host, prefix, stack, service, method, path, status, \
                       header_ms, total_ms, bytes, client, upstream_failed, request_id \
                       FROM requests";

impl Paged for RequestQuery {
    type Row = Row;
    type Item = RequestEntry;

    fn page(&self) -> PageAt {
        self.page
    }

    fn limit(&self) -> u32 {
        self.limit.clamp(1, LogQuery::MAX_LIMIT)
    }

    fn filters_in_rust(&self) -> bool {
        self.path.is_some()
    }

    fn select(&self) -> QueryBuilder<Sqlite> {
        let mut sql = QueryBuilder::new(COLUMNS);
        LogQuery::push_where(&mut sql, &self.range, &self.scope);
        LogQuery::push_in(&mut sql, "service", self.services.clone());
        if let Some(status) = self.status {
            status.push_sql(&mut sql);
        }
        if let Some(min) = self.min_total {
            sql.push(" AND total_ms >= ")
                .push_bind(LogStore::millis(min));
        }
        if let Some(id) = self.request_id {
            sql.push(" AND request_id = ").push_bind(id.to_string());
        }
        sql
    }

    fn decode(row: Row) -> Result<RequestEntry, UnknownLogText> {
        RequestEntry::try_from(row)
    }

    fn key(entry: &RequestEntry) -> RecordKey {
        entry.key()
    }

    fn matches(&self, entry: &RequestEntry) -> bool {
        self.path
            .as_ref()
            .is_none_or(|path| path.is_match(&entry.request.path))
    }
}

/// A request row as stored, with its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestEntry {
    pub id: i64,
    pub request: RequestRow,
}

impl RequestEntry {
    /// Its place in page order.
    pub fn key(&self) -> RecordKey {
        RecordKey {
            time: self.request.time,
            id: self.id,
        }
    }
}

type Row = (
    i64,
    Timestamp,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    i64,
    i64,
    String,
    bool,
    String,
);

impl TryFrom<Row> for RequestEntry {
    type Error = UnknownLogText;

    fn try_from(row: Row) -> Result<Self, Self::Error> {
        let (
            id,
            time,
            host,
            prefix,
            stack,
            service,
            method,
            path,
            status,
            header_ms,
            total_ms,
            bytes,
            client,
            upstream_failed,
            request_id,
        ) = row;
        let millis = |ms: i64| Duration::from_millis(u64::try_from(ms).unwrap_or(0));
        Ok(Self {
            id,
            request: RequestRow {
                time,
                host,
                prefix,
                stack,
                service,
                method,
                path,
                status: u16::try_from(status)
                    .map_err(|_| UnknownLogText::new("status", &status.to_string()))?,
                to_header: header_ms.map(millis),
                total: millis(total_ms),
                bytes: u64::try_from(bytes).unwrap_or(0),
                client: client
                    .parse()
                    .map_err(|_| UnknownLogText::new("client address", &client))?,
                upstream_failed,
                request_id: request_id.parse()?,
            },
        })
    }
}

/// How many requests a range holds, for the footer of the access-log source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestCounts {
    pub rows: u64,
    /// Answered with a 4xx.
    pub client_errors: u64,
    /// Answered with a 5xx.
    pub server_errors: u64,
}

impl LogStore {
    /// One page of the request rows `query` matches, with the same paging and scan cap as
    /// [`LogStore::records`]. Reads the read pool only.
    pub async fn requests(&self, query: &RequestQuery) -> Result<Page<RequestEntry>> {
        self.page(query).await
    }

    /// The requests of `query`'s range, scope and services, and how many of them were 4xx and
    /// 5xx: what the page's other filters choose among. Counted with the index, no scan cap.
    pub async fn request_counts(&self, query: &RequestQuery) -> Result<RequestCounts> {
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT count(*), \
                    coalesce(sum(status >= 400 AND status < 500), 0), \
                    coalesce(sum(status >= 500 AND status < 600), 0) \
             FROM requests",
        );
        LogQuery::push_where(&mut sql, &query.range, &query.scope);
        LogQuery::push_in(&mut sql, "service", query.services.clone());
        let (rows, client_errors, server_errors): (i64, i64, i64) =
            sql.build_query_as().fetch_one(&self.reads).await?;
        let count = |n: i64| u64::try_from(n).unwrap_or(0);
        Ok(RequestCounts {
            rows: count(rows),
            client_errors: count(client_errors),
            server_errors: count(server_errors),
        })
    }

    /// The row of the request with `id`, whenever it arrived; `None` once retention removed
    /// it, or for an id the proxy never gave. A container record that logged a request id
    /// links here.
    pub async fn request(&self, id: RequestId) -> Result<Option<RequestEntry>> {
        let mut sql = QueryBuilder::<Sqlite>::new(COLUMNS);
        sql.push(" WHERE request_id = ").push_bind(id.to_string());
        let row: Option<Row> = sql.build_query_as().fetch_optional(&self.reads).await?;
        row.map(RequestEntry::try_from)
            .transpose()
            .map_err(|e| StoreError::Database(sqlx::Error::Decode(Box::new(e))))
    }
}
