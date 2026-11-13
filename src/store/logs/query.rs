//! The read side of the log store (ADR-0067 §8): [`LogStore::records`] reads a page of
//! `log_records` for a [`LogQuery`], and [`LogStore::requests`] a page of `requests` for a
//! [`RequestQuery`]. Both narrow by time and scope in SQL, with the `(stack, time)` or `(time)`
//! index, and by whatever else SQL can compare; text, regular expressions and field filters are
//! matched in Rust, a chunk of rows at a time, until the page is full or [`SCAN_CAP`] rows have
//! been visited. The two share that scan, the [`PageAt`] cursor and the [`Page`] they return.

mod requests;

use std::fmt;
use std::ops::Range;
use std::str::FromStr;
use std::time::Duration;

use regex::{Regex, RegexBuilder};
use sqlx::sqlite::SqliteRow;
use sqlx::{QueryBuilder, Sqlite};

use super::{Level, LogRecord, LogStore, Stream, UnknownLogText};
use crate::store::{Result, StoreError, Timestamp};

pub use requests::{RequestCounts, RequestEntry, RequestQuery, StatusFilter};

/// The most rows one query visits. A query that reaches it before its page is full stops and
/// says where ([`Page::scan_stopped`]).
pub const SCAN_CAP: u32 = 50_000;

/// Whose records a query reads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LogScope {
    /// Every record: every stack's and hoster's own (the Telemetry page).
    #[default]
    All,
    /// One stack's records, hoster's own about that stack included (ADR-0068 §5).
    Stack(String),
    /// hoster's own records that name no stack: `hoster` in the Telemetry stack selector.
    Hoster,
}

impl LogScope {
    fn push_sql(&self, sql: &mut QueryBuilder<Sqlite>) {
        match self {
            LogScope::All => {}
            LogScope::Stack(stack) => {
                sql.push(" AND stack = ").push_bind(stack.clone());
            }
            LogScope::Hoster => {
                sql.push(" AND stack IS NULL");
            }
        }
    }
}

/// A regular expression that does not compile, or compiles too large.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid regular expression: {0}")]
pub struct InvalidPattern(regex::Error);

/// What a record's text has to contain: its message, or one of its fields written as
/// `key=value` with the dotted key of [`LogRecord::flat_fields`].
#[derive(Debug, Clone)]
pub enum TextFilter {
    /// A substring, ignoring case. Held lowercased.
    Substring(String),
    /// A regular expression, as written (`(?i)` makes it ignore case).
    Regex(Regex),
}

impl TextFilter {
    /// The most memory a compiled expression may take, so a pattern from the UI cannot make
    /// hoster build an enormous automaton.
    pub const REGEX_SIZE_LIMIT: usize = 1 << 20;

    /// Matches `text` anywhere, ignoring case.
    pub fn substring(text: &str) -> Self {
        Self::Substring(text.to_lowercase())
    }

    /// Compiles `pattern` under [`TextFilter::REGEX_SIZE_LIMIT`].
    pub fn regex(pattern: &str) -> Result<Self, InvalidPattern> {
        RegexBuilder::new(pattern)
            .size_limit(Self::REGEX_SIZE_LIMIT)
            .dfa_size_limit(Self::REGEX_SIZE_LIMIT)
            .build()
            .map(Self::Regex)
            .map_err(InvalidPattern)
    }

    /// Whether it matches anywhere in `haystack`.
    pub fn is_match(&self, haystack: &str) -> bool {
        match self {
            TextFilter::Substring(needle) => haystack.to_lowercase().contains(needle.as_str()),
            TextFilter::Regex(regex) => regex.is_match(haystack),
        }
    }

    /// Where it matches in `haystack`, as byte ranges on character boundaries, in order and
    /// without overlaps or empty matches: what the logs view marks in a message.
    pub fn find_all(&self, haystack: &str) -> Vec<Range<usize>> {
        match self {
            TextFilter::Regex(regex) => regex
                .find_iter(haystack)
                .filter(|m| !m.is_empty())
                .map(|m| m.range())
                .collect(),
            TextFilter::Substring(needle) if needle.is_empty() => Vec::new(),
            TextFilter::Substring(needle) => {
                let mut found = Vec::new();
                let mut from = 0;
                while let Some(c) = haystack[from..].chars().next() {
                    match Self::lowercase_prefix(&haystack[from..], needle) {
                        Some(len) => {
                            found.push(from..from + len);
                            from += len;
                        }
                        None => from += c.len_utf8(),
                    }
                }
                found
            }
        }
    }

    /// The length in bytes of the start of `text` whose lowercase is `needle`, if it has one.
    fn lowercase_prefix(text: &str, needle: &str) -> Option<usize> {
        let mut rest = needle;
        for (at, c) in text.char_indices() {
            if rest.is_empty() {
                return Some(at);
            }
            for lower in c.to_lowercase() {
                rest = rest.strip_prefix(lower)?;
            }
        }
        rest.is_empty().then_some(text.len())
    }
}

/// A record's place in the order pages use: its time, then its id for equal times. Written as
/// `<time>.<id>`, e.g. in an "older" link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordKey {
    pub time: Timestamp,
    pub id: i64,
}

impl fmt::Display for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.time.as_millis(), self.id)
    }
}

impl FromStr for RecordKey {
    type Err = UnknownLogText;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || UnknownLogText::new("record key", s);
        let (time, id) = s.split_once('.').ok_or_else(invalid)?;
        Ok(Self {
            time: Timestamp::from_millis(time.parse().map_err(|_| invalid())?),
            id: id.parse().map_err(|_| invalid())?,
        })
    }
}

/// Where a page starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageAt {
    /// The newest matching records of the range.
    #[default]
    Newest,
    /// The newest matching records older than this key (the "older" link, "search older").
    Before(RecordKey),
    /// The oldest matching records newer than this key (the "newer" link, following).
    After(RecordKey),
}

/// Which records a page of the log store shows.
#[derive(Debug, Clone)]
pub struct LogQuery {
    /// Records stamped at or after the start and before the end.
    pub range: Range<Timestamp>,
    pub scope: LogScope,
    /// Only these services; every service when empty.
    pub services: Vec<String>,
    /// Only records at these levels; every record, those without a level included, when
    /// empty. A record without a level matches only the empty set (ADR-0067: text lines have no
    /// level).
    pub levels: Vec<Level>,
    /// Only these streams; every stream when empty.
    pub streams: Vec<Stream>,
    pub text: Option<TextFilter>,
    /// `(dotted key, value)` pairs a record's [`flat_fields`](LogRecord::flat_fields) must all
    /// hold exactly.
    pub fields: Vec<(String, String)>,
    pub page: PageAt,
    /// At most this many records; clamped to `1..=`[`LogQuery::MAX_LIMIT`].
    pub limit: u32,
}

impl LogQuery {
    /// Records per page unless the caller asks otherwise.
    pub const DEFAULT_LIMIT: u32 = 200;
    /// The most records a page holds.
    pub const MAX_LIMIT: u32 = 1000;
    /// The most rows one query visits: [`SCAN_CAP`].
    pub const SCAN_CAP: u32 = SCAN_CAP;

    /// The newest page of every record in `range` and `scope`, unfiltered.
    pub fn new(range: Range<Timestamp>, scope: LogScope) -> Self {
        Self {
            range,
            scope,
            services: Vec::new(),
            levels: Vec::new(),
            streams: Vec::new(),
            text: None,
            fields: Vec::new(),
            page: PageAt::Newest,
            limit: Self::DEFAULT_LIMIT,
        }
    }

    /// The filters SQL cannot apply.
    fn matches_record(&self, record: &LogRecord) -> bool {
        let flat = record.flat_fields();
        let fields_hold = self
            .fields
            .iter()
            .all(|wanted| flat.iter().any(|field| field == wanted));
        fields_hold
            && self.text.as_ref().is_none_or(|text| {
                text.is_match(&record.message)
                    || flat
                        .iter()
                        .any(|(key, value)| text.is_match(&format!("{key}={value}")))
            })
    }

    /// `SELECT <the record columns> FROM <table> WHERE …` with every filter SQL applies.
    fn select_from(&self, table: &str) -> QueryBuilder<Sqlite> {
        let mut sql = QueryBuilder::new(format!(
            "SELECT id, time, stack, service, generation, stream, level, message, fields, cut, \
             marker FROM {table}"
        ));
        Self::push_where(&mut sql, &self.range, &self.scope);
        Self::push_in(&mut sql, "service", self.services.clone());
        Self::push_in(&mut sql, "level", self.levels.iter().map(|l| l.as_str()));
        Self::push_in(&mut sql, "stream", self.streams.iter().map(|s| s.as_str()));
        sql
    }

    /// The narrowing by range and scope that every read of either table starts with.
    fn push_where(sql: &mut QueryBuilder<Sqlite>, range: &Range<Timestamp>, scope: &LogScope) {
        sql.push(" WHERE time >= ")
            .push_bind(range.start)
            .push(" AND time < ")
            .push_bind(range.end);
        scope.push_sql(sql);
    }

    /// `AND <column> IN (…)`, or nothing for no values.
    fn push_in<T>(
        sql: &mut QueryBuilder<Sqlite>,
        column: &'static str,
        values: impl IntoIterator<Item = T>,
    ) where
        T: 'static + Send + sqlx::Encode<'static, Sqlite> + sqlx::Type<Sqlite>,
    {
        let mut values = values.into_iter().peekable();
        if values.peek().is_none() {
            return;
        }
        sql.push(" AND ").push(column).push(" IN (");
        let mut list = sql.separated(", ");
        for value in values {
            list.push_bind(value);
        }
        sql.push(")");
    }
}

impl Paged for LogQuery {
    type Row = EntryRow;
    type Item = LogEntry;

    fn page(&self) -> PageAt {
        self.page
    }

    fn limit(&self) -> u32 {
        self.limit.clamp(1, Self::MAX_LIMIT)
    }

    fn filters_in_rust(&self) -> bool {
        self.text.is_some() || !self.fields.is_empty()
    }

    fn select(&self) -> QueryBuilder<Sqlite> {
        self.select_from("log_records")
    }

    fn decode(row: EntryRow) -> Result<LogEntry, UnknownLogText> {
        LogEntry::try_from(row)
    }

    fn key(entry: &LogEntry) -> RecordKey {
        entry.key()
    }

    fn matches(&self, entry: &LogEntry) -> bool {
        self.matches_record(&entry.record)
    }
}

/// A query the shared scan pages through one table with, by `(time, id)`.
trait Paged {
    /// A row as SQL returns it.
    type Row: for<'r> sqlx::FromRow<'r, SqliteRow> + Send + Unpin;
    /// A row as the page holds it.
    type Item;

    fn page(&self) -> PageAt;
    /// The page size, clamped.
    fn limit(&self) -> u32;
    /// Whether some rows SQL returns may still not match.
    fn filters_in_rust(&self) -> bool;
    /// `SELECT <columns> FROM <table> WHERE …` with every filter SQL applies, starting with
    /// [`LogQuery::push_where`]; the scan adds the cursor, the order and the limit.
    fn select(&self) -> QueryBuilder<Sqlite>;
    fn decode(row: Self::Row) -> Result<Self::Item, UnknownLogText>;
    fn key(item: &Self::Item) -> RecordKey;
    /// The filters SQL cannot apply; only asked when [`Paged::filters_in_rust`].
    fn matches(&self, item: &Self::Item) -> bool;

    /// Rows fetched per round trip while filtering in Rust.
    const CHUNK: u32 = 1000;

    /// One chunk's `SELECT`: up to `rows` rows past `from` (exclusive), towards older ones or
    /// newer.
    fn chunk_sql(&self, from: Option<RecordKey>, older: bool, rows: u32) -> QueryBuilder<Sqlite> {
        let mut sql = self.select();
        if let Some(key) = from {
            // The plain `time` bound lets the index narrow; the row value breaks ties by id.
            let (cmp, bound) = if older { ("<", "<=") } else { (">", ">=") };
            sql.push(format_args!(" AND time {bound} "))
                .push_bind(key.time)
                .push(format_args!(" AND (time, id) {cmp} ("))
                .push_bind(key.time)
                .push(", ")
                .push_bind(key.id)
                .push(")");
        }
        sql.push(if older {
            " ORDER BY time DESC, id DESC LIMIT "
        } else {
            " ORDER BY time ASC, id ASC LIMIT "
        })
        .push_bind(i64::from(rows));
        sql
    }
}

/// A record as stored, with its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub id: i64,
    pub record: LogRecord,
}

impl LogEntry {
    /// Its place in page order.
    pub fn key(&self) -> RecordKey {
        RecordKey {
            time: self.record.time,
            id: self.id,
        }
    }
}

type EntryRow = (
    i64,
    Timestamp,
    Option<String>,
    String,
    Option<i64>,
    String,
    Option<String>,
    String,
    Option<String>,
    bool,
    bool,
);

impl TryFrom<EntryRow> for LogEntry {
    type Error = UnknownLogText;

    fn try_from(row: EntryRow) -> Result<Self, Self::Error> {
        let (id, time, stack, service, generation, stream, level, message, fields, cut, marker) =
            row;
        Ok(Self {
            id,
            record: LogRecord {
                time,
                stack,
                service,
                generation: generation.and_then(|g| u32::try_from(g).ok()),
                stream: stream.parse()?,
                level: level.as_deref().map(str::parse).transpose()?,
                message,
                fields,
                cut,
                marker,
            },
        })
    }
}

/// One page of records or request rows, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// Ordered by time, then id.
    pub entries: Vec<T>,
    /// There may be matching records older than the page. Always true for a page read
    /// [`After`](PageAt::After) a key.
    pub older: bool,
    /// There may be matching records newer than the page. Always true for a page read
    /// [`Before`](PageAt::Before) a key, false for the [`Newest`](PageAt::Newest).
    pub newer: bool,
    /// The scan hit [`SCAN_CAP`] before the page was full: the last row it visited, so the UI
    /// can offer to search on from there (`Before` or `After` it).
    pub scan_stopped: Option<RecordKey>,
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            older: false,
            newer: false,
            scan_stopped: None,
        }
    }
}

/// A page of `log_records`.
pub type LogPage = Page<LogEntry>;

/// The records committed after an id that a query matches: what a follow stream sends next
/// ([`LogStore::records_after`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogTail {
    /// In id order, which is commit order.
    pub entries: Vec<LogEntry>,
    /// Where the next read starts: past every id examined, matching or not.
    pub through: i64,
    /// It stopped at its limit or its window before the head: more may follow at once.
    pub more: bool,
}

/// A service with records in a range: see [`LogStore::services_in`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSeen {
    /// `None` for hoster's own records that name no stack.
    pub stack: Option<String>,
    pub service: String,
    /// The deployment generations of its records, lowest first; empty when none has one.
    pub generations: Vec<u32>,
}

/// How many records of each level one time bucket holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelCounts {
    /// The bucket's start: a multiple of its length since the epoch.
    pub start: Timestamp,
    levels: [u64; Level::ALL.len()],
    unleveled: u64,
}

impl LevelCounts {
    /// The records at `level`, or without a level for `None`.
    pub fn count(&self, level: Option<Level>) -> u64 {
        match level {
            Some(level) => self.levels[level as usize],
            None => self.unleveled,
        }
    }

    /// Every record of the bucket.
    pub fn total(&self) -> u64 {
        self.levels.iter().sum::<u64>() + self.unleveled
    }
}

impl LogStore {
    /// One page of the records `query` matches. Reads the read pool only, so it never waits
    /// for the recorder.
    pub async fn records(&self, query: &LogQuery) -> Result<LogPage> {
        self.page(query).await
    }

    /// The records with an id above `after`, up to `head` (the newest committed id, from the
    /// recorder's announcement), that `query` matches, in id order: for following the store as
    /// the recorder commits (ADR-0067 §4, "the id serves … following"). The query's range,
    /// scope and filters apply, its page does not. One call returns at most the query's limit
    /// of records and examines at most [`SCAN_CAP`] ids past `after`; the cursor it returns is
    /// past every id it examined, matching or not, so a quiet scope beside a busy one does not
    /// read the same rows again. Reads by id, never by the `(stack, time)` index, so a read
    /// finds only what was committed since `after`.
    pub async fn records_after(&self, query: &LogQuery, after: i64, head: i64) -> Result<LogTail> {
        let limit = query.limit() as usize;
        let in_rust = query.filters_in_rust();
        // The ids this call examines: a bounded window, whatever the filters let through.
        let end = head.min(after.saturating_add(i64::from(SCAN_CAP)));
        let mut tail = LogTail {
            through: after,
            ..LogTail::default()
        };
        while tail.through < end {
            let rows = if in_rust {
                <LogQuery as Paged>::CHUNK
            } else {
                u32::try_from(limit - tail.entries.len()).unwrap_or(u32::MAX)
            };
            // `NOT INDEXED` keeps the planner on the rowid range instead of the time index,
            // which would read the whole range for the few new rows.
            let mut sql = query.select_from("log_records NOT INDEXED");
            sql.push(" AND id > ")
                .push_bind(tail.through)
                .push(" AND id <= ")
                .push_bind(end)
                .push(" ORDER BY id LIMIT ")
                .push_bind(i64::from(rows));
            let chunk: Vec<EntryRow> = sql.build_query_as().fetch_all(&self.reads).await?;
            let fetched = u32::try_from(chunk.len()).unwrap_or(u32::MAX);
            for row in chunk {
                let entry = LogEntry::try_from(row)
                    .map_err(|e| StoreError::Database(sqlx::Error::Decode(Box::new(e))))?;
                tail.through = entry.id;
                if !in_rust || query.matches_record(&entry.record) {
                    tail.entries.push(entry);
                    if tail.entries.len() == limit {
                        // Rows after this one are read next time.
                        tail.more = tail.through < head;
                        return Ok(tail);
                    }
                }
            }
            if fetched < rows {
                // Nothing else in the window matches.
                break;
            }
        }
        tail.through = tail.through.max(end);
        tail.more = tail.through < head;
        Ok(tail)
    }

    /// One page of what `query` matches, with the shared scan.
    async fn page<Q: Paged>(&self, query: &Q) -> Result<Page<Q::Item>> {
        let limit = query.limit();
        let (mut from, older) = match query.page() {
            PageAt::Newest => (None, true),
            PageAt::Before(key) => (Some(key), true),
            PageAt::After(key) => (Some(key), false),
        };
        // One more than the page shows tells whether there is more beyond it.
        let wanted = limit as usize + 1;
        let mut found = Vec::with_capacity(wanted);
        let mut scanned = 0;
        let mut exhausted = false;
        let in_rust = query.filters_in_rust();
        while found.len() < wanted && scanned < SCAN_CAP {
            let rows = if in_rust {
                Q::CHUNK.min(SCAN_CAP - scanned)
            } else {
                u32::try_from(wanted - found.len())
                    .unwrap_or(u32::MAX)
                    .min(SCAN_CAP - scanned)
            };
            let chunk: Vec<Q::Row> = query
                .chunk_sql(from, older, rows)
                .build_query_as()
                .fetch_all(&self.reads)
                .await?;
            let fetched = u32::try_from(chunk.len()).unwrap_or(u32::MAX);
            scanned += fetched;
            for row in chunk {
                let entry = Q::decode(row)
                    .map_err(|e| StoreError::Database(sqlx::Error::Decode(Box::new(e))))?;
                from = Some(Q::key(&entry));
                if !in_rust || query.matches(&entry) {
                    found.push(entry);
                    if found.len() == wanted {
                        break;
                    }
                }
            }
            if fetched < rows {
                exhausted = true;
                break;
            }
        }
        let more = found.len() == wanted;
        found.truncate(limit as usize);
        let scan_stopped = (!more && !exhausted).then_some(from).flatten();
        let beyond = more || scan_stopped.is_some();
        if older {
            found.reverse();
        }
        Ok(Page {
            entries: found,
            older: if older { beyond } else { true },
            newer: if older {
                query.page() != PageAt::Newest
            } else {
                beyond
            },
            scan_stopped,
        })
    }

    /// How many records without a level `query` would list if it took every level: what a
    /// level filter hides (ADR-0067 §3). Narrows by range, scope, services and streams only,
    /// with the index and no message scan, so its text and field filters and its page are not
    /// applied.
    pub async fn count_unleveled(&self, query: &LogQuery) -> Result<u64> {
        let mut sql = QueryBuilder::<Sqlite>::new("SELECT count(*) FROM log_records");
        LogQuery::push_where(&mut sql, &query.range, &query.scope);
        LogQuery::push_in(&mut sql, "service", query.services.clone());
        LogQuery::push_in(&mut sql, "stream", query.streams.iter().map(|s| s.as_str()));
        sql.push(" AND level IS NULL");
        let count: i64 = sql.build_query_scalar().fetch_one(&self.reads).await?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// When the oldest stored record was stamped: where the logs start once the size cap has
    /// cut history (ADR-0067 §7). `None` when there are none.
    pub async fn oldest_record_time(&self) -> Result<Option<Timestamp>> {
        Ok(sqlx::query_scalar("SELECT min(time) FROM log_records")
            .fetch_one(&self.reads)
            .await?)
    }

    /// Every stack and service with records in `range` and `scope`, with the deployment
    /// generations those records came from: the service chips of the logs view, and whether a
    /// row needs its generation to tell replaced containers apart. Ordered by stack (hoster's
    /// own records without one first), then service. Reads what the index narrows to, with no
    /// message scan, as [`LogStore::count_by_level`] does.
    pub async fn services_in(
        &self,
        range: Range<Timestamp>,
        scope: &LogScope,
    ) -> Result<Vec<ServiceSeen>> {
        let mut sql =
            QueryBuilder::<Sqlite>::new("SELECT stack, service, generation FROM log_records");
        LogQuery::push_where(&mut sql, &range, scope);
        sql.push(" GROUP BY stack, service, generation ORDER BY stack, service, generation");
        let rows: Vec<(Option<String>, String, Option<i64>)> =
            sql.build_query_as().fetch_all(&self.reads).await?;
        let mut seen: Vec<ServiceSeen> = Vec::new();
        for (stack, service, generation) in rows {
            if seen
                .last()
                .is_none_or(|last| last.stack != stack || last.service != service)
            {
                seen.push(ServiceSeen {
                    stack,
                    service,
                    generations: Vec::new(),
                });
            }
            let last = seen.last_mut().expect("pushed above");
            if let Some(generation) = generation.and_then(|g| u32::try_from(g).ok()) {
                last.generations.push(generation);
            }
        }
        Ok(seen)
    }

    /// Records per `bucket`-long period since the epoch and level in `range` and `scope`, for
    /// the severity histogram, oldest bucket first. Buckets without records are left out. Reads
    /// what the index narrows to, with no message scan; a zero `bucket` counts per millisecond.
    pub async fn count_by_level(
        &self,
        range: Range<Timestamp>,
        scope: &LogScope,
        bucket: Duration,
    ) -> Result<Vec<LevelCounts>> {
        let length = i64::try_from(bucket.as_millis()).unwrap_or(i64::MAX).max(1);
        let mut sql = QueryBuilder::<Sqlite>::new("SELECT time - ((time % ");
        sql.push_bind(length)
            .push(" + ")
            .push_bind(length)
            .push(") % ")
            .push_bind(length)
            .push(") AS bucket, level, count(*) FROM log_records");
        LogQuery::push_where(&mut sql, &range, scope);
        sql.push(" GROUP BY bucket, level ORDER BY bucket");
        let rows: Vec<(Timestamp, Option<String>, i64)> =
            sql.build_query_as().fetch_all(&self.reads).await?;

        let mut buckets: Vec<LevelCounts> = Vec::new();
        for (start, level, count) in rows {
            let level = level
                .as_deref()
                .map(Level::from_str)
                .transpose()
                .map_err(|e| StoreError::Database(sqlx::Error::Decode(Box::new(e))))?;
            if buckets.last().is_none_or(|last| last.start != start) {
                buckets.push(LevelCounts {
                    start,
                    levels: [0; Level::ALL.len()],
                    unleveled: 0,
                });
            }
            let counts = buckets.last_mut().expect("pushed above");
            let count = u64::try_from(count).unwrap_or(0);
            match level {
                Some(level) => counts.levels[level as usize] = count,
                None => counts.unleveled = count,
            }
        }
        Ok(buckets)
    }
}
