//! The stored logs view (ADR-0067 §4, §7, §8; ADR-0068): a page of the log store's records,
//! every service merged oldest to newest (by time, then record id), with a service column,
//! a *since* range and paging back through history, replaced and removed containers included.
//!
//! One view, two scopes ([`LogScope`]): the stack logs tab mounts it with its stack fixed
//! (`/stacks/{name}/logs`), and the Telemetry page's Logs tab (`web/telemetry.rs`) mounts it
//! across every stack, with a stack column, a stack selector (`stack`: one stack, or
//! [`LogScope::HOSTER`] for hoster's own records without one) and the range's record count. A
//! mount says where the view lives and what it knows beyond the store ([`LogMount`]: the
//! services the current revision declares, the containers whose live log it links), and
//! [`LogsView::load`] reads the store and builds what `logs_view.html` renders.
//!
//! Every option is a plain GET parameter ([`LogParams`]): `service`, `since` (a preset, a
//! duration or a UTC time; by default 1h of a stack, 15m across stacks), `level` ([`LevelFilter`]), `q` and `regex=on` (grep:
//! a substring ignoring case, or a regular expression, of the message and the fields), `field`
//! (repeatable, [`FieldFilter`]: `<key>=<value>`, an exact match on a flattened field) and the
//! page cursor `before`/`after`, a record's `<time>.<id>` key. Every link keeps the other
//! options, and the form keeps them as hidden fields. A page holds at most
//! [`LogsView::PAGE_SIZE`] records; "older" and "newer" page by record key, never by offset, so
//! equal times page correctly.
//!
//! Level and grep run in the store's query (ADR-0067 §8), which scans at most
//! [`LogQuery::SCAN_CAP`] rows: when it stops before the page is full the view says how far it
//! searched and links "search older" from there. Grep matches are marked in the message. A
//! level filter hides the records without a level (text lines), and the toolbar says how many.
//! A pattern that does not compile is shown beside the field, with no records.
//!
//! A record with fields (a JSON line's other keys, ADR-0067 §3) opens in place, a `<details>`
//! with no script, on its fields flattened to dotted keys and what the row leaves out: the full
//! time, the stream, the generation and whether the line was cut. Each value links the view
//! with that value as a field filter; active ones are removable chips in the toolbar, and all
//! must match. The store runs them in the same scan as grep, under the same cap. A record whose
//! fields grep matched opens by itself, and `tracing`'s `target` is shown in the row.
//!
//! A mount with a stream ([`LogMount::stream`], the stack tab) follows on the newest end of the
//! range: the record list connects to [`LogFollow`], which appends what the recorder commits
//! after the page, by record id, with the page's options. `follow=off` switches it off (the
//! switch's links keep it), and paging back into history stops it; "newer" and "newest" turn it
//! back on.
//!
//! The stack scope has a source switch ([`LogSource`], `source`): the containers' records
//! from the log store (the default, everything here), or another source that pages its own
//! entries in the shape of the rows ([`SourceEntry`], [`LogsView::listing`], [`SourceFollow`]),
//! taking only the shared options (the range, grep, paging, following). Switching keeps the
//! options the new source takes, and the page cursors never.
//!
//! hoster's own records that name the stack are listed under the service `hoster`, with a chip
//! of its own. A JSON line's level is shown; a text line has none (ADR-0067 §3), and a
//! `stderr` line is set apart as on the live log. The marker record for dropped lines (§7) is
//! a notice row. When the store's oldest record is newer than the start of the range (the
//! size cap, or a young server), the view says from when its data starts. When the store
//! cannot be read, it says so and the mount's live log links stay (§10).
//!
//! A mount with a download ([`LogMount::download`], the stack tab) offers the records its
//! options select over the whole range as a file, in each [`LogFormat`]: [`LogDownload`]
//! (`logs/download.rs`) plans the same store query and reads it page by page.

use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use askama::Template;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::containers::{LogEntry as LiveLine, LogSince, SinceLink};
use super::system::LogStoreState;
use super::ui::Count;
mod download;

pub use download::{Download, FormatParam, LogDownload, Refused};

use crate::store::{
    self, Level, LogEntry, LogHead, LogPage, LogQuery, LogRecord, LogStore, Page, PageAt,
    RecordKey, ServiceSeen, Stream, TextFilter, Timestamp,
};

/// Whose records the view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogScope {
    /// One stack's: the stack logs tab.
    Stack(String),
    /// Every stack's, or the one picked (`stack`): the Telemetry page's Logs tab.
    /// [`LogScope::HOSTER`] picks hoster's own records that name no stack.
    All { stack: Option<String> },
}

impl LogScope {
    /// The `stack` that picks hoster's own records without a stack (ADR-0068): `hoster` in the
    /// selector. Stack names start with a letter, so no stack can be called this.
    pub const HOSTER: &str = "_hoster";

    /// The range without a `since`: the last hour of one stack, the last 15 minutes of all.
    pub fn default_since(&self) -> &'static str {
        match self {
            LogScope::Stack(_) => "1h",
            LogScope::All { .. } => "15m",
        }
    }

    /// The stack picked across stacks, which every link keeps as `stack`.
    fn picked(&self) -> Option<&str> {
        match self {
            LogScope::Stack(_) => None,
            LogScope::All { stack } => stack.as_deref(),
        }
    }

    /// The sources its switch offers; none (no switch) across stacks.
    pub fn sources(&self) -> &'static [LogSource] {
        match self {
            LogScope::Stack(_) => &LogSource::SWITCH,
            LogScope::All { .. } => &[],
        }
    }

    /// The store's scope for this one.
    fn store(&self) -> store::LogScope {
        match self {
            LogScope::All { stack: Some(stack) } if stack == Self::HOSTER => {
                store::LogScope::Hoster
            }
            LogScope::Stack(stack) | LogScope::All { stack: Some(stack) } => {
                store::LogScope::Stack(stack.clone())
            }
            LogScope::All { stack: None } => store::LogScope::All,
        }
    }

    /// Whether rows name their stack: only across stacks.
    fn stack_column(&self) -> bool {
        matches!(self, LogScope::All { stack: None })
    }
}

/// Where the records of a view come from (`source`): a switch of the stack scope only. Each
/// source takes its own of the shared options ([`LogSource::takes`]); switching keeps those the
/// new one takes and drops the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogSource {
    /// The log store's records of the containers, and hoster's own.
    #[default]
    Containers,
    /// The stack's deployment events, read from `hoster.db` rather than the log store, so not
    /// bound by its retention (`web/stacks/logs/deploy.rs`).
    Deploy,
}

impl LogSource {
    /// The switch, in order. The access log (task bq6) slots in between.
    pub const SWITCH: [LogSource; 2] = [LogSource::Containers, LogSource::Deploy];

    /// The `source` parameter, and what its switch says.
    pub fn as_str(self) -> &'static str {
        match self {
            LogSource::Containers => "containers",
            LogSource::Deploy => "deploy",
        }
    }

    /// Whether it takes the GET parameter `name`.
    fn takes(self, name: &str) -> bool {
        match self {
            LogSource::Containers => true,
            LogSource::Deploy => !matches!(name, "service" | "level" | "field"),
        }
    }

    /// The grep field's accessible name: what grep matches.
    pub fn grep_label(self) -> &'static str {
        match self {
            LogSource::Containers => "Grep: text in the message or fields",
            LogSource::Deploy => "Grep: text in the message",
        }
    }

    /// When the list grows while following.
    pub fn follow_note(self) -> &'static str {
        match self {
            LogSource::Containers => {
                "new records are added as hoster stores them, about once a second"
            }
            LogSource::Deploy => "new deployment events are added as the deployments log them",
        }
    }
}

impl fmt::Display for LogSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_ascii_lowercase();
        Self::SWITCH
            .into_iter()
            .find(|source| source.as_str() == lower)
            .ok_or_else(|| format!("`{s}` is not a log source; showing the containers' records."))
    }
}

/// The log store as the pages read it: the [`LogStoreState`] hoster opened at start, and the
/// recorder's announcement of what it committed, which follow streams wait on.
#[derive(Debug, Clone)]
pub struct LogReader {
    state: LogStoreState,
    /// The newest committed ids, sent after every commit (ADR-0067 §6).
    head: watch::Receiver<LogHead>,
    /// Cancelled when the admin app stops: follow streams end then, so they do not hold its
    /// graceful shutdown open.
    closing: CancellationToken,
}

impl Default for LogReader {
    /// No log store (tests that do not read logs).
    fn default() -> Self {
        let (_, head) = watch::channel(LogHead::default());
        Self::new(
            LogStoreState::Unavailable("it was not opened".to_owned()),
            head,
        )
    }
}

impl LogReader {
    /// The store in `state`, whose recorder announces its commits on `head`.
    pub fn new(state: LogStoreState, head: watch::Receiver<LogHead>) -> Self {
        Self {
            state,
            head,
            closing: CancellationToken::new(),
        }
    }

    /// The same reader, whose follow streams end when `token` is cancelled.
    pub fn closing_with(self, token: CancellationToken) -> Self {
        Self {
            closing: token,
            ..self
        }
    }

    /// The open store, or why there is none.
    pub(super) fn store(&self) -> Result<&LogStore, String> {
        match &self.state {
            LogStoreState::Open(store) => Ok(store),
            LogStoreState::Unavailable(error) => {
                Err(format!("The log store is unavailable: {error}"))
            }
        }
    }

    /// The newest committed record id.
    fn head(&self) -> i64 {
        self.head.borrow().record
    }

    /// What one page shows: its records, the services of its range, where the data starts and
    /// what a level filter hides.
    async fn read(&self, plan: &Plan) -> Result<Read, String> {
        let store = self.store()?;
        // Read before the page: a record committed in between is on the page or followed
        // after it, never neither.
        let head = self.head();
        let query = &plan.query;
        let records = async {
            match plan.invalid {
                Some(_) => Ok(LogPage::default()),
                None => store.records(query).await,
            }
        };
        let unleveled = async {
            match plan.counts_unleveled() {
                true => store.count_unleveled(query).await.map(Some),
                false => Ok(None),
            }
        };
        // Across stacks: the stacks of the range for the selector, and the records it holds.
        let stacks = async {
            match plan.across {
                true => store
                    .stacks_in(query.range.clone(), LogsView::STACKS_CAP)
                    .await
                    .map(Some),
                false => Ok(None),
            }
        };
        let total = async {
            match plan.across {
                true => store
                    .count_records(query.range.clone(), &query.scope, LogsView::COUNT_CAP + 1)
                    .await
                    .map(Some),
                false => Ok(None),
            }
        };
        let read = async {
            let (seen, page, oldest, unleveled, stacks, total) = tokio::try_join!(
                store.services_in(query.range.clone(), &query.scope),
                records,
                store.oldest_record_time(),
                unleveled,
                stacks,
                total,
            )?;
            Ok::<_, store::StoreError>(Read {
                seen,
                page,
                oldest,
                unleveled,
                head,
                stacks: stacks.unwrap_or_default(),
                total,
            })
        };
        read.await.map_err(|err| {
            tracing::error!(error = %err, "reading the log store failed");
            format!("Cannot read the log store: {:#}", anyhow::Error::from(err))
        })
    }
}

/// What the store answered for one page.
#[derive(Debug, Default)]
struct Read {
    seen: Vec<ServiceSeen>,
    page: LogPage,
    oldest: Option<Timestamp>,
    /// The records without a level the level filter hides, when that was counted.
    unleveled: Option<u64>,
    /// The newest committed record id before the page was read.
    head: i64,
    /// Across stacks: the stacks with records in the range.
    stacks: Vec<String>,
    /// Across stacks: the records of the range and scope, counted up to one past
    /// [`LogsView::COUNT_CAP`].
    total: Option<u64>,
}

/// The store query one page runs, and what was wrong with its options.
#[derive(Debug)]
struct Plan {
    query: LogQuery,
    /// An option that did not parse and was replaced by its default.
    problem: Option<String>,
    /// Why the grep pattern does not compile; no records are read then.
    invalid: Option<String>,
    /// Across stacks: the stack selector and the record count are read too.
    across: bool,
}

impl Plan {
    /// Whether to count what the level filter hides: only when the count is one indexed read,
    /// i.e. no text or field filter would have to scan for it.
    fn counts_unleveled(&self) -> bool {
        !self.query.levels.is_empty()
            && self.query.text.is_none()
            && self.query.fields.is_empty()
            && self.invalid.is_none()
    }
}

/// What every source reads for a page: the range, grep and where the page starts, from the
/// shared options; the log store's [`Plan`] adds its own to them, and a source other than the
/// log store reads them as they are ([`LogsView::listing`]).
#[derive(Debug)]
pub struct SourcePlan {
    /// Entries stamped at or after the start and before the end.
    pub range: std::ops::Range<Timestamp>,
    pub text: Option<TextFilter>,
    pub page: PageAt,
    /// At most this many entries: [`LogsView::PAGE_SIZE`].
    pub limit: u32,
    /// An option that did not parse and was replaced by its default.
    problem: Option<String>,
    /// Why the grep pattern does not compile; nothing is read then.
    invalid: Option<String>,
}

impl SourcePlan {
    /// What `params` ask for at `now`.
    pub fn new(params: &LogParams, now: SystemTime) -> Self {
        let (start, since_problem) = params.start(now);
        let (page, page_problem) = params.page();
        let end = Timestamp::from(now) + LogsView::END_SLACK;
        let (text, invalid) = match params.grep() {
            Ok(text) => (text, None),
            Err(message) => (None, Some(message)),
        };
        Self {
            range: start..end.max(start),
            text,
            page,
            limit: LogsView::PAGE_SIZE,
            problem: since_problem.or(page_problem),
            invalid,
        }
    }

    /// Whether the page reads its source: not when the grep pattern does not compile.
    pub fn reads(&self) -> bool {
        self.invalid.is_none()
    }
}

/// The level chips: which levels a page lists. Records without a level only under
/// [`LevelFilter::All`] (ADR-0067 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LevelFilter {
    #[default]
    All,
    Error,
    /// `warn` and `error`.
    Warn,
    Info,
    /// `debug` and `trace`.
    Debug,
}

impl LevelFilter {
    /// Every chip, in toolbar order.
    pub const CHIPS: [LevelFilter; 5] = [
        LevelFilter::All,
        LevelFilter::Error,
        LevelFilter::Warn,
        LevelFilter::Info,
        LevelFilter::Debug,
    ];

    /// The `level` parameter.
    fn as_str(self) -> &'static str {
        match self {
            LevelFilter::All => "all",
            LevelFilter::Error => "error",
            LevelFilter::Warn => "warn",
            LevelFilter::Info => "info",
            LevelFilter::Debug => "debug",
        }
    }

    /// What its chip says.
    pub fn label(self) -> &'static str {
        match self {
            LevelFilter::Warn => "warn+",
            other => other.as_str(),
        }
    }

    /// The colour class of its chip's dot.
    fn class(self) -> &'static str {
        match self {
            LevelFilter::All => "lvl-all",
            LevelFilter::Error => "lvl-error",
            LevelFilter::Warn => "lvl-warn",
            LevelFilter::Info => "lvl-info",
            LevelFilter::Debug => "lvl-debug",
        }
    }

    /// The store's levels for it; empty for every record.
    fn levels(self) -> Vec<Level> {
        match self {
            LevelFilter::All => Vec::new(),
            LevelFilter::Error => vec![Level::Error],
            LevelFilter::Warn => vec![Level::Warn, Level::Error],
            LevelFilter::Info => vec![Level::Info],
            LevelFilter::Debug => vec![Level::Trace, Level::Debug],
        }
    }
}

impl fmt::Display for LevelFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for LevelFilter {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "warn+" => Ok(LevelFilter::Warn),
            "trace" => Ok(LevelFilter::Debug),
            lower => Self::CHIPS
                .into_iter()
                .find(|chip| chip.as_str() == lower)
                .ok_or_else(|| format!("`{s}` is not a level; showing every level.")),
        }
    }
}

/// A field filter: records whose [flattened fields](crate::store::LogRecord::flat_fields)
/// hold `value` under `key` exactly.
///
/// As a `field` parameter it is `<key>=<value>`, split on the first `=` that is not encoded:
/// the key is written with `%` and `=` encoded (`%25`, `%3D`), so a key holding `=` round
/// trips, and the value is written as it is. A typed `user.id=42` is the key `user.id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldFilter {
    pub key: String,
    pub value: String,
}

impl FieldFilter {
    /// `key` with `%` and `=` encoded.
    fn encode(key: &str) -> String {
        key.replace('%', "%25").replace('=', "%3D")
    }

    /// The inverse of [`FieldFilter::encode`]; any other `%` is kept as it is.
    fn decode(key: &str) -> String {
        let mut decoded = String::with_capacity(key.len());
        let mut rest = key;
        while let Some(at) = rest.find('%') {
            decoded.push_str(&rest[..at]);
            rest = &rest[at..];
            let escape = rest.get(..3);
            if escape == Some("%25") {
                decoded.push('%');
                rest = &rest[3..];
            } else if escape.is_some_and(|e| e.eq_ignore_ascii_case("%3D")) {
                decoded.push('=');
                rest = &rest[3..];
            } else {
                decoded.push('%');
                rest = &rest[1..];
            }
        }
        decoded.push_str(rest);
        decoded
    }
}

impl fmt::Display for FieldFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", Self::encode(&self.key), self.value)
    }
}

impl FromStr for FieldFilter {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (key, value) = s.split_once('=').ok_or_else(|| {
            format!("`{s}` is not a field filter (`key=value`); showing every record.")
        })?;
        Ok(Self {
            key: Self::decode(key),
            value: value.to_owned(),
        })
    }
}

impl From<FieldFilter> for (String, String) {
    fn from(filter: FieldFilter) -> Self {
        (filter.key, filter.value)
    }
}

/// The options of the view, as GET parameters. Every field is text, as an HTML form sends it.
/// Read from the query's pairs in order ([`From`]), so `field` can repeat; of any other
/// parameter given twice, the first counts.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(from = "Vec<(String, String)>")]
pub struct LogParams {
    /// Where the records come from: a [`LogSource`]; the containers when absent.
    source: Option<String>,
    /// Only this service; every service when absent or empty.
    service: Option<String>,
    /// Where the range starts: a preset, a duration ago (`2h`) or a UTC time.
    since: Option<String>,
    /// The page of records older than this key (`<time>.<id>`).
    before: Option<String>,
    /// The page of records newer than this key.
    after: Option<String>,
    /// Only these levels: a [`LevelFilter`]; every record when absent.
    level: Option<String>,
    /// Grep: what the message or fields contain. Not trimmed: spaces are searched for.
    q: Option<String>,
    /// `on`: `q` is a regular expression.
    regex: Option<String>,
    /// `off`: the newest page does not follow new records. Following is on otherwise.
    follow: Option<String>,
    /// The follow stream's cursor: the newest record id the page has seen.
    from: Option<String>,
    /// Field filters ([`FieldFilter`]), as given.
    field: Vec<String>,
    /// Across stacks, the stack picked ([`LogScope::All`]); the stack tab fixes it by its path.
    stack: Option<String>,
    /// The range without a `since`, set by the scope ([`LogParams::scoped`]); not a parameter.
    default_since: Option<String>,
}

impl From<Vec<(String, String)>> for LogParams {
    fn from(pairs: Vec<(String, String)>) -> Self {
        let mut params = Self::default();
        for (key, value) in pairs {
            let slot = match key.as_str() {
                "source" => &mut params.source,
                "service" => &mut params.service,
                "since" => &mut params.since,
                "before" => &mut params.before,
                "after" => &mut params.after,
                "level" => &mut params.level,
                "q" => &mut params.q,
                "regex" => &mut params.regex,
                "follow" => &mut params.follow,
                "from" => &mut params.from,
                "stack" => &mut params.stack,
                "field" => {
                    params.field.push(value);
                    continue;
                }
                _ => continue,
            };
            slot.get_or_insert(value);
        }
        params
    }
}

impl LogParams {
    /// The *since* presets, shared by both scopes.
    pub const SINCE_PRESETS: [&str; 6] = ["5m", "15m", "1h", "6h", "24h", "7d"];
    /// The range without a `since`, unless the scope sets its own ([`LogScope::default_since`]).
    pub const DEFAULT_SINCE: &str = "1h";

    /// The same options, with the range `scope` defaults to.
    pub fn scoped(self, scope: &LogScope) -> Self {
        Self {
            default_since: Some(scope.default_since().to_owned()),
            ..self
        }
    }

    /// The stack picked across stacks (`stack`), when given.
    pub fn stack(&self) -> Option<&str> {
        Self::text(&self.stack)
    }

    /// The range without a `since`.
    fn default_since(&self) -> &str {
        self.default_since.as_deref().unwrap_or(Self::DEFAULT_SINCE)
    }

    fn text(value: &Option<String>) -> Option<&str> {
        value.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }

    fn service(&self) -> Option<&str> {
        Self::text(&self.service)
    }

    /// The *since* in effect, as typed.
    fn since(&self) -> &str {
        Self::text(&self.since).unwrap_or(self.default_since())
    }

    /// The source in effect in `scope`; the containers when `source` is absent, not one of the
    /// scope's (no switch) or does not parse, with why for the last.
    pub fn source(&self, scope: &LogScope) -> (LogSource, Option<String>) {
        let sources = scope.sources();
        match Self::text(&self.source).map(str::parse::<LogSource>) {
            None => (LogSource::default(), None),
            Some(Ok(source)) if sources.contains(&source) => (source, None),
            Some(Ok(_)) => (LogSource::default(), None),
            Some(Err(message)) => (LogSource::default(), Some(message)),
        }
    }

    /// The start of the range at `now`; the default's when `since` does not parse, with why.
    fn start(&self, now: SystemTime) -> (Timestamp, Option<String>) {
        let default = || {
            Timestamp::from(
                self.default_since()
                    .parse::<LogSince>()
                    .expect("the default parses")
                    .resolve(now),
            )
        };
        match self.since().parse::<LogSince>() {
            Ok(since) => (Timestamp::from(since.resolve(now)), None),
            Err(message) => (default(), Some(message)),
        }
    }

    /// Where the page starts; the newest records when the cursor does not parse, with why.
    fn page(&self) -> (PageAt, Option<String>) {
        let parse = |key: &str| {
            key.parse::<RecordKey>()
                .map_err(|_| format!("`{key}` is not a page of the log; showing the newest."))
        };
        let result = match (Self::text(&self.before), Self::text(&self.after)) {
            (Some(key), _) => parse(key).map(PageAt::Before),
            (None, Some(key)) => parse(key).map(PageAt::After),
            (None, None) => Ok(PageAt::Newest),
        };
        match result {
            Ok(page) => (page, None),
            Err(message) => (PageAt::Newest, Some(message)),
        }
    }

    /// The level filter; every level when `level` does not parse, with why.
    fn level(&self) -> (LevelFilter, Option<String>) {
        match Self::text(&self.level).map(str::parse).transpose() {
            Ok(level) => (level.unwrap_or_default(), None),
            Err(message) => (LevelFilter::All, Some(message)),
        }
    }

    /// The grep text as typed; an empty one is none.
    fn q(&self) -> Option<&str> {
        self.q.as_deref().filter(|q| !q.is_empty())
    }

    fn regex(&self) -> bool {
        Self::text(&self.regex).is_some_and(|v| v.eq_ignore_ascii_case("on"))
    }

    /// The store's text filter for `q` and `regex`, or why the pattern does not compile.
    fn grep(&self) -> Result<Option<TextFilter>, String> {
        let Some(q) = self.q() else {
            return Ok(None);
        };
        if self.regex() {
            TextFilter::regex(q)
                .map(Some)
                .map_err(|err| err.to_string())
        } else {
            Ok(Some(TextFilter::substring(q)))
        }
    }

    /// Whether the newest page follows new records: unless switched off.
    fn follows(&self) -> bool {
        !Self::text(&self.follow).is_some_and(|v| v.eq_ignore_ascii_case("off"))
    }

    /// The follow stream's cursor, when given and a record id.
    fn from(&self) -> Option<i64> {
        Self::text(&self.from).and_then(|id| id.parse().ok())
    }

    /// The field filters, each once, in the order given; those that do not parse are left out,
    /// with why.
    fn fields(&self) -> (Vec<FieldFilter>, Option<String>) {
        let mut fields: Vec<FieldFilter> = Vec::new();
        let mut problem = None;
        for given in self.field.iter().filter(|f| !f.is_empty()) {
            match given.parse::<FieldFilter>() {
                Ok(filter) if !fields.contains(&filter) => fields.push(filter),
                Ok(_) => {}
                Err(message) => {
                    problem.get_or_insert(message);
                }
            }
        }
        (fields, problem)
    }

    /// The options every link of `source` keeps: all but the page and stream cursors, only
    /// those given and those the source takes.
    fn filters(&self, source: LogSource) -> Vec<(&'static str, String)> {
        let level = self.level().0;
        let mut filters: Vec<(&'static str, String)> = [
            (
                "source",
                (source != LogSource::default()).then(|| source.as_str()),
            ),
            ("service", self.service()),
            ("since", Self::text(&self.since)),
            ("level", (level != LevelFilter::All).then(|| level.as_str())),
            ("q", self.q()),
            ("regex", self.regex().then_some("on")),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((key, value?.to_owned())))
        .collect();
        filters.extend(
            self.fields()
                .0
                .iter()
                .map(|field| ("field", field.to_string())),
        );
        if !self.follows() {
            filters.push(("follow", "off".to_owned()));
        }
        filters.retain(|(key, _)| source.takes(key));
        filters
    }

    /// Whether level, grep or a field filter narrow the page.
    fn filtered(&self) -> bool {
        self.level().0 != LevelFilter::All || self.q().is_some() || !self.fields().0.is_empty()
    }
}

/// Where a view is mounted, and what it knows beyond the store.
#[derive(Debug)]
pub struct LogMount {
    pub scope: LogScope,
    /// The page's path; links and the form go there.
    pub action: String,
    /// Services that get a chip even without records in the range: the current revision's, in
    /// its order.
    pub services: Vec<String>,
    /// Containers whose live log (`/containers/{name}/logs`) the toolbar links.
    pub live: Vec<LiveLink>,
    /// Where the newest page's follow stream is ([`LogFollow`]); `None` where the view does
    /// not follow.
    pub stream: Option<String>,
    /// Where the download of the view's records is ([`LogDownload`]); `None` where it offers
    /// none.
    pub download: Option<String>,
    /// Across stacks: the stacks the selector offers even without records in the range, the
    /// current state's.
    pub stacks: Vec<String>,
}

/// A link to one container's live log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveLink {
    pub name: String,
    /// What it is beyond its name: service, generation, role and state.
    pub detail: String,
}

impl LiveLink {
    pub fn href(&self) -> String {
        format!("/containers/{}/logs", self.name)
    }
}

/// The links of one view: its path and the options they keep.
#[derive(Debug, Clone)]
struct Links {
    action: String,
    filters: Vec<(&'static str, String)>,
}

impl Links {
    /// The links of a view at `action` in `scope` showing `source` with the options of
    /// `params`: those options, and the stack picked across stacks first.
    fn of(action: &str, scope: &LogScope, params: &LogParams, source: LogSource) -> Self {
        let mut filters = params.filters(source);
        if let Some(stack) = scope.picked() {
            filters.insert(0, ("stack", stack.to_owned()));
        }
        Self {
            action: action.to_owned(),
            filters,
        }
    }

    /// The view with `set` replacing those options (`None` drops one; `field` replaces every
    /// field filter), and no page cursor unless `set` names one.
    fn to(&self, set: &[(&'static str, Option<&str>)]) -> String {
        let mut pairs: Vec<(&str, &str)> = self
            .filters
            .iter()
            .filter(|(key, _)| set.iter().all(|(k, _)| k != key))
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        pairs.extend(set.iter().filter_map(|(k, v)| Some((*k, (*v)?))));
        self.href(pairs)
    }

    /// The view with the field filter `field` added, and no page cursor.
    fn with_field(&self, field: &FieldFilter) -> String {
        let field = field.to_string();
        let mut pairs = self.pairs();
        if !pairs.contains(&("field", field.as_str())) {
            // Beside the other field filters, before the follow switch.
            let at = pairs
                .iter()
                .position(|(k, _)| *k == "follow")
                .unwrap_or(pairs.len());
            pairs.insert(at, ("field", &field));
        }
        self.href(pairs)
    }

    /// The view without the field filter `field`, and no page cursor.
    fn without_field(&self, field: &FieldFilter) -> String {
        let field = field.to_string();
        let mut pairs = self.pairs();
        pairs.retain(|pair| *pair != ("field", field.as_str()));
        self.href(pairs)
    }

    fn pairs(&self) -> Vec<(&str, &str)> {
        self.filters.iter().map(|(k, v)| (*k, v.as_str())).collect()
    }

    fn href(&self, pairs: Vec<(&str, &str)>) -> String {
        if pairs.is_empty() {
            return self.action.clone();
        }
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        format!("{}?{query}", self.action)
    }
}

/// An option of the stack selector, across stacks.
#[derive(Debug, PartialEq, Eq)]
pub struct StackOption {
    /// The `stack` parameter; empty for every stack.
    pub value: String,
    pub label: String,
    pub current: bool,
}

/// How many records the range holds, for the Telemetry page's tab: counted up to
/// [`LogsView::COUNT_CAP`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Total(u64);

impl fmt::Display for Total {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cap = u64::from(LogsView::COUNT_CAP);
        match self.0 > cap {
            true => write!(f, "{}+", Count(cap)),
            false => write!(f, "{}", Count(self.0)),
        }
    }
}

/// A service or level chip of the toolbar, or "all".
#[derive(Debug)]
pub struct Chip {
    pub label: String,
    pub href: String,
    pub current: bool,
    /// The colour class of its dot and its rows.
    pub class: &'static str,
}

/// One row of the record list.
#[derive(Debug)]
pub struct LogRow {
    /// RFC 3339 in UTC, to the millisecond.
    pub time: String,
    /// The time as the row shows it: the time of day, with the date when the page spans
    /// more than today.
    pub shown: String,
    /// Across stacks only; `—` for hoster's own records without one.
    pub stack: Option<String>,
    pub service: String,
    /// Where the service column links: a deployment's page, for the deploy source.
    pub href: Option<String>,
    /// The deployment generation, when the service has more than one in the range.
    pub generation: Option<u32>,
    pub class: &'static str,
    pub stderr: bool,
    pub level: Option<Level>,
    pub message: String,
    /// Where grep matched in `message`, as byte ranges.
    pub marks: Vec<Range<usize>>,
    /// The line was cut at the store's length cap.
    pub cut: bool,
    /// A notice about lines hoster lost, not a log line.
    pub marker: bool,
    /// Shown dimmed before the message: `tracing`'s `target` field, or a deployment event's
    /// transition or step.
    pub target: Option<String>,
    /// What the row opens on; `None` for a record without fields, which does not open.
    pub detail: Option<RecordDetail>,
}

/// What a record with fields shows when opened, beside the time ([`LogRow::time`]) and the cut
/// mark ([`LogRow::cut`]) its row has.
#[derive(Debug)]
pub struct RecordDetail {
    pub stream: Stream,
    /// The container's deployment generation, whether or not the row names it.
    pub generation: Option<u32>,
    /// The fields flattened, by key.
    pub fields: Vec<FieldRow>,
    /// The fields as stored, when they no longer parse as a JSON object.
    pub raw: Option<String>,
    /// Grep matched in the fields: the record is shown open.
    pub open: bool,
}

/// One flattened field of an opened record.
#[derive(Debug)]
pub struct FieldRow {
    /// The dotted key, printable.
    pub key: String,
    /// The value, printable, its line breaks kept.
    pub value: String,
    /// Where grep matched in `value`, as byte ranges.
    pub marks: Vec<Range<usize>>,
    /// The view with this key and value as a field filter.
    pub href: String,
}

impl FieldRow {
    /// The value in runs, marked and not.
    pub fn spans(&self) -> Vec<Span<'_>> {
        Span::runs(&self.value, &self.marks)
    }
}

/// A run of a message's or a value's text, marked where grep matched.
#[derive(Debug, PartialEq, Eq)]
pub struct Span<'a> {
    pub text: &'a str,
    pub mark: bool,
}

impl<'a> Span<'a> {
    /// `text` in runs, marked at `marks` (in order, without overlaps) and not, for the
    /// template to escape one by one.
    fn runs(text: &'a str, marks: &[Range<usize>]) -> Vec<Self> {
        let mut spans = Vec::new();
        let mut at = 0;
        for mark in marks {
            if mark.start > at {
                spans.push(Span {
                    text: &text[at..mark.start],
                    mark: false,
                });
            }
            spans.push(Span {
                text: &text[mark.clone()],
                mark: true,
            });
            at = mark.end;
        }
        if at < text.len() || spans.is_empty() {
            spans.push(Span {
                text: &text[at..],
                mark: false,
            });
        }
        spans
    }
}

impl LogRow {
    /// The message in runs, marked and not, for the template to escape one by one.
    pub fn spans(&self) -> Vec<Span<'_>> {
        Span::runs(&self.message, &self.marks)
    }
}

/// An active field filter's chip in the toolbar.
#[derive(Debug)]
pub struct FieldChip {
    /// `key=value`, printable.
    pub label: String,
    /// The view without it.
    pub remove: String,
}

/// How records become rows: the same on a page and in its follow stream.
#[derive(Debug, Clone)]
struct Rows {
    /// The services in chip order, which picks their colours.
    services: Vec<String>,
    /// The generations seen of each stack's service: a row names its own when there are
    /// several.
    generations: HashMap<(Option<String>, String), BTreeSet<u32>>,
    /// Rows name their stack.
    stack_column: bool,
    /// Rows show the date as well as the time of day.
    dated: bool,
    /// What grep marks in the messages.
    text: Option<TextFilter>,
    /// The page's links, which a field value adds its filter to.
    links: Links,
}

impl Rows {
    /// The field that names a `tracing` event's module.
    const TARGET: &str = "target";

    /// Rows for `services` (in chip order), with the generations in `seen`, linking `links`.
    fn new(services: Vec<String>, seen: &[ServiceSeen], links: Links) -> Self {
        let generations = seen
            .iter()
            .map(|s| {
                (
                    (s.stack.clone(), s.service.clone()),
                    s.generations.iter().copied().collect(),
                )
            })
            .collect();
        Self {
            services,
            generations,
            stack_column: false,
            dated: false,
            text: None,
            links,
        }
    }

    /// The colour class of `service`'s chip and rows.
    fn class(&self, service: &str) -> &'static str {
        if service == LogsView::HOSTER {
            return LogsView::HOSTER_CLASS;
        }
        let n = self.services.iter().position(|s| s == service).unwrap_or(0);
        LogsView::PALETTE[n % LogsView::PALETTE.len()]
    }

    /// Notes the generation of `entry`, which a stream sees after the page was read.
    fn learn(&mut self, entry: &LogEntry) {
        let record = &entry.record;
        if let Some(generation) = record.generation {
            self.generations
                .entry((record.stack.clone(), record.service.clone()))
                .or_default()
                .insert(generation);
        }
    }

    fn row(&self, entry: &LogEntry) -> LogRow {
        let record = &entry.record;
        let message = LiveLine::printable(&record.message);
        let time = LogsView::utc(record.time);
        let shown = if self.dated {
            time[5..23].replacen('T', " ", 1)
        } else {
            time[11..23].to_owned()
        };
        let several = self
            .generations
            .get(&(record.stack.clone(), record.service.clone()))
            .is_some_and(|g| g.len() > 1);
        let mut row = LogRow {
            shown,
            time,
            stack: self
                .stack_column
                .then(|| record.stack.clone().unwrap_or_else(|| "—".to_owned())),
            service: record.service.clone(),
            href: None,
            generation: record.generation.filter(|_| several),
            class: self.class(&record.service),
            stderr: record.stream == Stream::Stderr,
            level: record.level,
            marks: match (&self.text, record.marker) {
                (Some(text), false) => text.find_all(&message),
                _ => Vec::new(),
            },
            message,
            cut: record.cut,
            marker: record.marker,
            target: None,
            detail: None,
        };
        if !record.marker {
            self.detail(record, &mut row);
        }
        row
    }

    /// The row of `entry`, from a source other than the log store.
    fn source_row(&self, entry: &SourceEntry) -> LogRow {
        let message = LiveLine::printable(&entry.message);
        let time = LogsView::utc(entry.key.time);
        let shown = if self.dated {
            time[5..23].replacen('T', " ", 1)
        } else {
            time[11..23].to_owned()
        };
        LogRow {
            shown,
            time,
            stack: self.stack_column.then(|| "—".to_owned()),
            service: LiveLine::printable(&entry.label),
            href: entry.href.clone(),
            generation: None,
            class: LogsView::PALETTE[entry.tone % LogsView::PALETTE.len()],
            stderr: false,
            level: entry.level,
            marks: self
                .text
                .as_ref()
                .map(|text| text.find_all(&message))
                .unwrap_or_default(),
            message,
            cut: false,
            marker: false,
            target: entry.step.as_deref().map(LiveLine::printable),
            detail: None,
        }
    }

    /// `text` printable, its line breaks kept: a field value may span lines (a stack trace,
    /// pretty JSON), which an opened record shows as they are.
    fn lines(text: &str) -> String {
        text.split('\n')
            .map(LiveLine::printable)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The target and what `record` opens on, from its fields, parsed once.
    fn detail(&self, record: &LogRecord, row: &mut LogRow) {
        let (flat, raw) = match record.parsed_fields() {
            None => return,
            Some(Ok(flat)) if flat.is_empty() => return,
            Some(Ok(flat)) => (flat, None),
            Some(Err(raw)) => (Vec::new(), Some(Self::lines(raw))),
        };
        row.target = flat
            .iter()
            .find(|(key, _)| key == Self::TARGET)
            .map(|(_, value)| LiveLine::printable(value));
        let open = self.text.as_ref().is_some_and(|text| {
            flat.iter()
                .any(|(key, value)| text.is_match(&format!("{key}={value}")))
        });
        let fields = flat
            .into_iter()
            .map(|(key, value)| {
                let href = self.links.with_field(&FieldFilter {
                    key: key.clone(),
                    value: value.clone(),
                });
                let value = Self::lines(&value);
                FieldRow {
                    key: LiveLine::printable(&key),
                    marks: self
                        .text
                        .as_ref()
                        .map(|text| text.find_all(&value))
                        .unwrap_or_default(),
                    value,
                    href,
                }
            })
            .collect();
        row.detail = Some(RecordDetail {
            stream: record.stream,
            generation: record.generation,
            fields,
            raw,
            open,
        });
    }
}

/// How a page pages: its "older", "newer" and "newest" links, where a capped scan stopped, and
/// whether it is the newest end of the range, which follows.
#[derive(Debug)]
struct Paging {
    older: Option<String>,
    newer: Option<String>,
    /// Back to the newest page, from an older or newer one.
    newest: Option<String>,
    stopped: Option<Stopped>,
    at_newest: bool,
}

impl Paging {
    /// The paging of `page`, read at `at`, its entries in order by `key`.
    fn new<T>(links: &Links, at: PageAt, page: &Page<T>, key: impl Fn(&T) -> RecordKey) -> Self {
        let first = page.entries.first().map(|e| key(e).to_string());
        let last = page.entries.last().map(|e| key(e).to_string());
        let reading_older = !matches!(at, PageAt::After(_));
        let stopped = page.scan_stopped.map(|key| {
            let cursor = key.to_string();
            Stopped {
                at: humantime::format_rfc3339_seconds(key.time.into()).to_string(),
                href: links.to(&[(
                    if reading_older { "before" } else { "after" },
                    Some(&cursor),
                )]),
                older: reading_older,
            }
        });
        // Where the scan stopped, "search older" (or newer) replaces paging from the last row.
        let older = first
            .filter(|_| page.older && !(stopped.is_some() && reading_older))
            .map(|key| links.to(&[("before", Some(&key))]));
        // Paging towards the newest page turns following back on.
        let newer = last
            .filter(|_| page.newer && !(stopped.is_some() && !reading_older))
            .map(|key| links.to(&[("after", Some(&key)), ("follow", None)]));
        let at_newest = match at {
            PageAt::Newest => true,
            PageAt::After(_) => !page.newer && stopped.is_none(),
            PageAt::Before(_) => false,
        };
        Self {
            older,
            newer,
            newest: (at != PageAt::Newest).then(|| links.to(&[("follow", None)])),
            stopped,
            at_newest,
        }
    }

    /// The follow switch, where the mount has a `stream` and the page is the newest end of a
    /// `readable` range, its stream starting after `cursor`.
    fn follow(
        &mut self,
        stream: Option<&String>,
        links: &Links,
        params: &LogParams,
        readable: bool,
        cursor: impl FnOnce() -> String,
    ) -> Option<Follow> {
        let follow = stream.filter(|_| self.at_newest && readable).map(|stream| {
            let cursor = cursor();
            let stream = Links {
                action: stream.clone(),
                filters: links.filters.clone(),
            };
            Follow {
                stream: params
                    .follows()
                    .then(|| stream.to(&[("from", Some(&cursor))])),
                on: links.to(&[("follow", None)]),
                off: links.to(&[("follow", Some("off"))]),
            }
        });
        // While following, the list drops its oldest rows, which "older" would skip: the view
        // switched off (the newest page again) has it.
        if follow.as_ref().is_some_and(Follow::following) {
            self.older = None;
        }
        follow
    }
}

/// An entry of a source other than the log store (the deploy source), in the shape of the
/// view's rows ([`LogRow`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    /// Its place in page order, and the follow stream's cursor.
    pub key: RecordKey,
    /// The service column: what the entry belongs to.
    pub label: String,
    /// Where the label links.
    pub href: Option<String>,
    /// Picks the label's colour from the service palette.
    pub tone: usize,
    pub level: Option<Level>,
    /// The step the entry is about, shown dimmed before the message.
    pub step: Option<String>,
    pub message: String,
}

/// What a source other than the log store shows for an empty range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyNote {
    pub text: String,
    /// The link after the text: what it says, and where it goes.
    pub link: String,
    pub href: String,
}

/// The scan stopped at its cap before the page was full.
#[derive(Debug)]
pub struct Stopped {
    /// The time of the last record it read, RFC 3339 in UTC to the second.
    pub at: String,
    /// Searching on from there.
    pub href: String,
    /// It was reading towards older records ("search older"), not newer.
    pub older: bool,
}

impl Stopped {
    /// The most records one search reads, as the page says it.
    pub fn cap(&self) -> Count {
        Count(u64::from(LogQuery::SCAN_CAP))
    }
}

/// The toolbar's note that the level filter hides the records without a level.
#[derive(Debug)]
pub struct Unleveled {
    /// How many, when the store could count them cheaply.
    pub count: Option<u64>,
    /// The view with every level.
    pub all: String,
}

impl Unleveled {
    /// The note's sentence.
    pub fn says(&self) -> String {
        match self.count {
            Some(1) => {
                "1 record without a level (a text line) is hidden by the level filter.".to_owned()
            }
            Some(n) => {
                format!(
                    "{} records without a level (text lines) are hidden by the level filter.",
                    Count(n)
                )
            }
            None => {
                "Records without a level (text lines) are hidden by the level filter.".to_owned()
            }
        }
    }
}

/// What `logs_view.html` renders.
#[derive(Debug)]
pub struct LogsView {
    /// Where the records come from.
    pub source: LogSource,
    /// The source switch, current one marked; empty without one.
    pub sources: Vec<Chip>,
    /// The stack selector, across stacks; empty in the stack scope.
    pub stacks: Vec<StackOption>,
    /// Across stacks: the records the range holds.
    pub total: Option<Total>,
    pub action: String,
    /// The options the form keeps as hidden fields (all but those it edits).
    pub keep: Vec<(&'static str, String)>,
    /// The *since* field as typed (empty for the default).
    pub since: String,
    pub presets: Vec<SinceLink>,
    pub chips: Vec<Chip>,
    pub levels: Vec<Chip>,
    /// The grep field as typed.
    pub q: String,
    /// The `.*` toggle is on.
    pub regex: bool,
    /// Why the grep pattern does not compile.
    pub invalid: Option<String>,
    /// The active field filters, all of which a record matches.
    pub fields: Vec<FieldChip>,
    /// What the level filter hides.
    pub unleveled: Option<Unleveled>,
    /// The scan stopped before the page was full.
    pub stopped: Option<Stopped>,
    pub live: Vec<LiveLink>,
    pub stack_column: bool,
    pub rows: Vec<LogRow>,
    pub older: Option<String>,
    pub newer: Option<String>,
    /// Back to the newest page, from an older or newer one.
    pub newest: Option<String>,
    /// From when the store holds records, when that is after the start of the range.
    pub starts_at: Option<String>,
    /// An option that did not parse and was replaced by its default.
    pub problem: Option<String>,
    /// Why the store could not be read.
    pub unreadable: Option<String>,
    /// The newest page of the range has no records.
    pub empty: bool,
    /// For an empty page under level or grep: what they are, and the view without them.
    pub cleared: Option<(String, String)>,
    /// What an empty range says, for a source other than the log store.
    pub empty_note: Option<EmptyNote>,
    /// The follow switch, at the newest end of the range of a view that follows.
    pub follow: Option<Follow>,
    /// The download button, where the mount has one: the containers' records only.
    pub download: Option<Download>,
}

/// The follow switch of the newest page, and its stream while it is on.
#[derive(Debug)]
pub struct Follow {
    /// The stream the record list connects to; `None` when switched off.
    pub stream: Option<String>,
    /// The view following.
    pub on: String,
    /// The view not following.
    pub off: String,
}

impl Follow {
    pub fn following(&self) -> bool {
        self.stream.is_some()
    }

    /// The most rows the list keeps while following, as the page says it.
    pub fn keeps(&self) -> Count {
        Count(LogFollow::KEEP as u64)
    }
}

impl LogsView {
    /// The most records a page shows.
    pub const PAGE_SIZE: u32 = 200;
    /// The colour classes of the service chips and rows, in turn by service order (ADR-0054:
    /// classes, never inline styles).
    /// They are the categorical colours of `docs/design/README.md` (Telemetry notes), hoster's
    /// own first ([`LogsView::HOSTER_CLASS`]).
    const PALETTE: [&str; 6] = ["svc-0", "svc-1", "svc-2", "svc-3", "svc-4", "svc-5"];
    /// hoster's own records (ADR-0068).
    const HOSTER_CLASS: &str = "svc-hoster";
    const ALL_CLASS: &str = "svc-all";
    /// The service hoster's own records are listed under.
    const HOSTER: &str = "hoster";
    /// The options the form edits rather than keeps as hidden fields.
    const EDITED: [&str; 4] = ["stack", "since", "q", "regex"];
    /// The most records the Telemetry page's tab counts; more are shown as this many and a `+`,
    /// so the count reads at most this many index entries.
    pub const COUNT_CAP: u32 = 100_000;
    /// The most stacks the stack selector lists from the store.
    const STACKS_CAP: u32 = 500;
    /// How far past now the range ends, so a line stamped as the page renders is on it.
    const END_SLACK: Duration = Duration::from_secs(1);

    /// The view of `mount` with the options of `params`, read from `reader`.
    pub async fn load(reader: &LogReader, mount: LogMount, params: &LogParams) -> Self {
        let now = SystemTime::now();
        let params = &params.clone().scoped(&mount.scope);
        let plan = Self::plan(&mount.scope, params, now);
        let read = reader.read(&plan).await;
        Self::new(mount, params, now, plan, read)
    }

    /// The store query `params` ask for at `now`, and what was wrong with them.
    fn plan(scope: &LogScope, params: &LogParams, now: SystemTime) -> Plan {
        let SourcePlan {
            range,
            text,
            page,
            limit,
            problem,
            invalid,
        } = SourcePlan::new(params, now);
        let (_, source_problem) = params.source(scope);
        let (level, level_problem) = params.level();
        let (fields, field_problem) = params.fields();
        let mut query = LogQuery::new(range, scope.store());
        query.services = params.service().map(str::to_owned).into_iter().collect();
        query.levels = level.levels();
        query.fields = fields.into_iter().map(Into::into).collect();
        query.text = text;
        query.page = page;
        query.limit = limit;
        Plan {
            query,
            problem: source_problem
                .or(problem)
                .or(level_problem)
                .or(field_problem),
            invalid,
            across: matches!(scope, LogScope::All { .. }),
        }
    }

    fn new(
        mount: LogMount,
        params: &LogParams,
        now: SystemTime,
        plan: Plan,
        read: Result<Read, String>,
    ) -> Self {
        let Plan {
            query,
            problem,
            invalid,
            ..
        } = plan;
        let links = Links::of(&mount.action, &mount.scope, params, LogSource::Containers);
        let (read, unreadable) = match read {
            Ok(read) => (read, None),
            Err(error) => (Read::default(), Some(error)),
        };
        let services = Self::services(&mount.services, &read.seen);
        let today = Self::utc(now.into())[..10].to_owned();
        let rows = Rows {
            stack_column: mount.scope.stack_column(),
            dated: !read
                .page
                .entries
                .iter()
                .all(|e| Self::utc(e.record.time).starts_with(&today)),
            text: query.text.clone(),
            ..Rows::new(services.clone(), &read.seen, links.clone())
        };
        let mut chips = vec![Chip {
            label: "all".to_owned(),
            href: links.to(&[("service", None)]),
            current: params.service().is_none(),
            class: Self::ALL_CLASS,
        }];
        chips.extend(services.iter().map(|service| Chip {
            label: service.clone(),
            href: links.to(&[("service", Some(service))]),
            current: params.service() == Some(service.as_str()),
            class: rows.class(service),
        }));

        let stack_column = rows.stack_column;
        let rendered: Vec<LogRow> = read.page.entries.iter().map(|e| rows.row(e)).collect();

        let page = &read.page;
        let mut paging = Paging::new(&links, query.page, page, LogEntry::key);
        let paged = query.page != PageAt::Newest;
        let (level, _) = params.level();
        let levels = LevelFilter::CHIPS
            .into_iter()
            .map(|chip| Chip {
                label: chip.label().to_owned(),
                href: links.to(&[("level", (chip != LevelFilter::All).then(|| chip.as_str()))]),
                current: chip == level,
                class: chip.class(),
            })
            .collect();
        let unleveled = (level != LevelFilter::All && invalid.is_none())
            .then(|| Unleveled {
                count: read.unleveled,
                all: links.to(&[("level", None)]),
            })
            .filter(|note| note.count != Some(0));
        let (field_filters, _) = params.fields();
        let fields = field_filters
            .iter()
            .map(|field| FieldChip {
                label: LiveLine::printable(&format!("{}={}", field.key, field.value)),
                remove: links.without_field(field),
            })
            .collect();
        let empty = rendered.is_empty() && !paged && unreadable.is_none() && invalid.is_none();
        let cleared = (empty && params.filtered()).then(|| {
            let mut active = Vec::new();
            if level != LevelFilter::All {
                active.push(format!("level {level}"));
            }
            if let Some(q) = params.q() {
                let kind = if params.regex() { "regex" } else { "grep" };
                active.push(format!("{kind} “{q}”"));
            }
            active.extend(
                field_filters
                    .iter()
                    .map(|field| format!("field {}={}", field.key, field.value)),
            );
            (
                active.join(" and "),
                links.to(&[
                    ("level", None),
                    ("q", None),
                    ("regex", None),
                    ("field", None),
                ]),
            )
        });
        let starts_at = read
            .oldest
            .filter(|oldest| *oldest > query.range.start)
            .map(|oldest| humantime::format_rfc3339_seconds(oldest.into()).to_string());
        let (since, presets) = Self::since(params, &links);
        let follow = paging.follow(
            mount.stream.as_ref(),
            &links,
            params,
            unreadable.is_none() && invalid.is_none(),
            || {
                // Past every record on the page, and every one committed before it was read.
                let newest = page.entries.iter().map(|e| e.id).max().unwrap_or(0);
                newest.max(read.head).to_string()
            },
        );
        let Paging {
            older,
            newer,
            newest,
            stopped,
            ..
        } = paging;
        let download = mount
            .download
            .as_ref()
            .map(|action| Download::of(action, &links));
        Self {
            download,
            source: LogSource::Containers,
            sources: Self::sources(&mount.scope, params, &mount.action, LogSource::Containers),
            stacks: Self::stacks(&mount.scope, &mount.stacks, &read.stacks),
            total: read.total.map(Total),
            empty_note: None,
            keep: links
                .filters
                .iter()
                .filter(|(key, _)| !Self::EDITED.contains(key))
                .cloned()
                .collect(),
            action: mount.action,
            since,
            presets,
            chips,
            levels,
            q: params.q().unwrap_or_default().to_owned(),
            regex: params.regex(),
            invalid,
            fields,
            unleveled,
            stopped,
            live: mount.live,
            stack_column,
            empty,
            cleared,
            follow,
            rows: rendered,
            older,
            newer,
            newest,
            starts_at,
            problem,
            unreadable,
        }
    }

    /// The source switch of `scope` at `action`, `current` marked: each source's view with the
    /// options of `current` it takes as well. Empty where the scope has no switch.
    fn sources(
        scope: &LogScope,
        params: &LogParams,
        action: &str,
        current: LogSource,
    ) -> Vec<Chip> {
        scope
            .sources()
            .iter()
            .map(|&source| {
                let mut filters = params.filters(current);
                filters.retain(|(key, _)| *key != "source" && source.takes(key));
                if source != LogSource::default() {
                    filters.insert(0, ("source", source.as_str().to_owned()));
                }
                Chip {
                    label: source.as_str().to_owned(),
                    href: Links {
                        action: action.to_owned(),
                        filters,
                    }
                    .to(&[]),
                    current: source == current,
                    class: Self::ALL_CLASS,
                }
            })
            .collect()
    }

    /// The stack selector of `scope`: every stack, then the stacks `known` and those with
    /// records in the range (`seen`) by name, then hoster's own; the one picked is current,
    /// and listed even when it is neither. Empty in the stack scope.
    fn stacks(scope: &LogScope, known: &[String], seen: &[String]) -> Vec<StackOption> {
        let LogScope::All { stack: picked } = scope else {
            return Vec::new();
        };
        let picked = picked.as_deref();
        let mut names: BTreeSet<&str> = known.iter().chain(seen).map(String::as_str).collect();
        names.extend(picked.filter(|p| *p != LogScope::HOSTER));
        let option = |value: &str, label: &str| StackOption {
            value: value.to_owned(),
            label: label.to_owned(),
            current: picked.unwrap_or_default() == value,
        };
        let mut options = vec![option("", "all stacks")];
        options.extend(names.into_iter().map(|name| option(name, name)));
        options.push(option(LogScope::HOSTER, Self::HOSTER));
        options
    }

    /// The *since* field as typed and its presets, linking `links`.
    fn since(params: &LogParams, links: &Links) -> (String, Vec<SinceLink>) {
        let since = LogParams::text(&params.since)
            .unwrap_or_default()
            .to_owned();
        let presets = LogParams::SINCE_PRESETS
            .into_iter()
            .map(|label| SinceLink {
                label,
                href: links.to(&[("since", Some(label))]),
                current: params.since() == label,
            })
            .collect();
        (since, presets)
    }

    /// The view of `mount` for `source`, a source other than the log store, with the options
    /// of `params` planned at `now` (`plan`) and the page that source `read` for them; `empty`
    /// is what an empty range says. Only the shared options apply: the range, grep, paging and
    /// following, which starts after the newest entry on the page.
    pub fn listing(
        mount: LogMount,
        params: &LogParams,
        now: SystemTime,
        source: LogSource,
        plan: SourcePlan,
        read: Result<Page<SourceEntry>, String>,
        empty: EmptyNote,
    ) -> Self {
        let links = Links::of(&mount.action, &mount.scope, params, source);
        let (page, unreadable) = match read {
            Ok(page) => (page, None),
            Err(error) => (Page::default(), Some(error)),
        };
        let today = Self::utc(now.into())[..10].to_owned();
        let rows = Rows {
            stack_column: mount.scope.stack_column(),
            dated: !page
                .entries
                .iter()
                .all(|e| Self::utc(e.key.time).starts_with(&today)),
            text: plan.text.clone(),
            ..Rows::new(Vec::new(), &[], links.clone())
        };
        let rendered: Vec<LogRow> = page.entries.iter().map(|e| rows.source_row(e)).collect();
        let mut paging = Paging::new(&links, plan.page, &page, |e| e.key);
        let invalid = plan.invalid;
        let follow = paging.follow(
            mount.stream.as_ref(),
            &links,
            params,
            unreadable.is_none() && invalid.is_none(),
            || {
                // Past every entry on the page: from the start of the range when it has none.
                page.entries
                    .last()
                    .map(|e| e.key)
                    .unwrap_or(RecordKey {
                        time: plan.range.start,
                        id: 0,
                    })
                    .to_string()
            },
        );
        let paged = plan.page != PageAt::Newest;
        let empty_page = rendered.is_empty() && !paged && unreadable.is_none() && invalid.is_none();
        let cleared = params.q().filter(|_| empty_page).map(|q| {
            let kind = if params.regex() { "regex" } else { "grep" };
            (
                format!("{kind} “{q}”"),
                links.to(&[("q", None), ("regex", None)]),
            )
        });
        let (since, presets) = Self::since(params, &links);
        let Paging {
            older,
            newer,
            newest,
            stopped,
            ..
        } = paging;
        Self {
            download: None,
            source,
            sources: Self::sources(&mount.scope, params, &mount.action, source),
            stacks: Self::stacks(&mount.scope, &mount.stacks, &[]),
            total: None,
            keep: links
                .filters
                .iter()
                .filter(|(key, _)| !Self::EDITED.contains(key))
                .cloned()
                .collect(),
            action: mount.action,
            since,
            presets,
            chips: Vec::new(),
            levels: Vec::new(),
            q: params.q().unwrap_or_default().to_owned(),
            regex: params.regex(),
            invalid,
            fields: Vec::new(),
            unleveled: None,
            stopped,
            live: mount.live,
            stack_column: rows.stack_column,
            empty: empty_page,
            cleared,
            empty_note: Some(empty),
            follow,
            rows: rendered,
            older,
            newer,
            newest,
            starts_at: None,
            problem: plan.problem,
            unreadable,
        }
    }

    /// The services that get a chip: `known` in order, then those with records in the range
    /// by name, `hoster` last.
    fn services(known: &[String], seen: &[ServiceSeen]) -> Vec<String> {
        let mut services: Vec<String> = known.to_vec();
        let mut others: Vec<&str> = seen
            .iter()
            .map(|s| s.service.as_str())
            .filter(|s| *s != Self::HOSTER && !known.iter().any(|k| k == s))
            .collect();
        others.sort_unstable();
        others.dedup();
        services.extend(others.into_iter().map(str::to_owned));
        if !services.iter().any(|s| s == Self::HOSTER)
            && seen.iter().any(|s| s.service == Self::HOSTER)
        {
            services.push(Self::HOSTER.to_owned());
        }
        services
    }

    /// `time` in RFC 3339, UTC, to the millisecond.
    fn utc(time: Timestamp) -> String {
        humantime::format_rfc3339_millis(time.into()).to_string()
    }

    /// The follow stream the record list connects to, while following.
    pub fn stream(&self) -> Option<&str> {
        self.follow.as_ref().and_then(|f| f.stream.as_deref())
    }

    /// The records the page shows, for its footer.
    pub fn count(&self) -> usize {
        self.rows.iter().filter(|r| !r.marker).count()
    }

    /// The records at `error` the page shows, for its footer.
    pub fn errors(&self) -> usize {
        self.at_level(Level::Error)
    }

    /// The records at `warn` the page shows, for its footer.
    pub fn warnings(&self) -> usize {
        self.at_level(Level::Warn)
    }

    fn at_level(&self, level: Level) -> usize {
        self.rows.iter().filter(|r| r.level == Some(level)).count()
    }
}

/// The follow stream of the newest page (ADR-0067 §6, ADR-0044): the records the recorder
/// commits after the page's newest, by record id, with the page's scope and options, as
/// `text/event-stream`.
///
/// It reads the store once each time the recorder announces a commit past its cursor, and
/// sends what matched as one message of rows ([`LogRecords`]), appended to the list, with the
/// last id read as the event ID, so the SSE extension resumes after it on a reconnect. A
/// burst is read [`LogFollow::CHUNK`] records at a time, one message each, without waiting
/// for the next commit. Every message also trims the list to its newest [`LogFollow::KEEP`]
/// rows and removes the page's "no records" notice.
///
/// It ends with a named `end` event, after a closing row, when the store cannot be read, when
/// the mount's `ends` resolves (the stack was removed, the session ended), when the recorder
/// stops or when the admin app shuts down. When the browser goes away, axum drops the stream
/// and nothing is left of it.
#[derive(Debug)]
pub struct LogFollow {
    store: LogStore,
    head: watch::Receiver<LogHead>,
    closing: CancellationToken,
    query: LogQuery,
    rows: Rows,
    /// The last record id read.
    cursor: i64,
}

impl LogFollow {
    /// The most records one message carries.
    pub const CHUNK: u32 = 200;
    /// The most rows the list keeps while following; older ones are removed.
    pub const KEEP: usize = 1_000;
    /// The named event after the last message; the page closes the connection on it.
    const END: &str = "end";
    /// How often an idle stream sends a comment, so a closed connection is noticed.
    const KEEP_ALIVE: Duration = Duration::from_secs(15);

    /// The stream of `mount` with the options of `params`, from the record id `last_event_id`
    /// (a reconnect), else the `from` the page gave, else the newest committed one.
    pub async fn open(
        reader: &LogReader,
        mount: &LogMount,
        params: &LogParams,
        last_event_id: Option<&str>,
    ) -> Result<Self, String> {
        let store = reader.store()?.clone();
        let now = SystemTime::now();
        let params = &params.clone().scoped(&mount.scope);
        let Plan {
            mut query, invalid, ..
        } = LogsView::plan(&mount.scope, params, now);
        if let Some(invalid) = invalid {
            return Err(invalid);
        }
        let seen = store
            .services_in(query.range.clone(), &query.scope)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "reading the log store failed");
                format!("Cannot read the log store: {:#}", anyhow::Error::from(err))
            })?;
        // Every record from the start of the range on, however far its time is ahead.
        query.range.end = Timestamp::from_millis(i64::MAX);
        query.limit = Self::CHUNK;
        let cursor = last_event_id
            .and_then(|id| id.trim().parse().ok())
            .or(params.from())
            .unwrap_or_else(|| reader.head());
        Ok(Self {
            store,
            head: reader.head.clone(),
            closing: reader.closing.clone(),
            rows: Rows {
                stack_column: mount.scope.stack_column(),
                text: query.text.clone(),
                // Field values link the page, as on the page itself.
                ..Rows::new(
                    LogsView::services(&mount.services, &seen),
                    &seen,
                    Links::of(&mount.action, &mount.scope, params, LogSource::Containers),
                )
            },
            query,
            cursor,
        })
    }

    /// The response for `opened`: its stream until `ends` resolves with the closing row's
    /// text, or the error as the one row of a stream that ends at once.
    pub fn respond(
        opened: Result<Self, String>,
        ends: impl Future<Output = String> + Send + 'static,
    ) -> Response {
        let events = match opened {
            Ok(follow) => follow.stream(ends).boxed(),
            Err(error) => stream::iter(Self::closing(&error)).map(Ok).boxed(),
        };
        Sse::new(events)
            .keep_alive(KeepAlive::new().interval(Self::KEEP_ALIVE))
            .into_response()
    }

    /// The closing row and the `end` event.
    fn closing(notice: &str) -> Vec<Event> {
        LogRecords::notice(notice)
            .event(None)
            .into_iter()
            .chain([Event::default().event(Self::END).data("")])
            .collect()
    }

    fn stream(
        self,
        ends: impl Future<Output = String> + Send + 'static,
    ) -> impl futures_util::Stream<Item = Result<Event, Infallible>> + Send + 'static {
        let ends: Pin<Box<dyn Future<Output = String> + Send>> = Box::pin(ends);
        // `more`: the last read stopped at its limit, so the next one does not wait.
        stream::unfold(Some((self, ends, false)), |state| async move {
            let (mut follow, mut ends, mut more) = state?;
            loop {
                if !more {
                    let cursor = follow.cursor;
                    let mut head = follow.head.clone();
                    let committed = async move {
                        head.wait_for(|head| head.record > cursor).await.map(|_| ())
                    };
                    let notice = tokio::select! {
                        committed = committed => committed.err().map(|_| {
                            "The log recorder stopped: reload the page to follow again.".to_owned()
                        }),
                        notice = &mut ends => Some(notice),
                        () = follow.closing.cancelled() => {
                            Some("hoster is shutting down: reload the page once it is back.".to_owned())
                        }
                    };
                    if let Some(notice) = notice {
                        return Some((Self::closing(&notice), None));
                    }
                }
                let head = follow.head.borrow().record;
                let tail = match follow
                    .store
                    .records_after(&follow.query, follow.cursor, head)
                    .await
                {
                    Ok(tail) => tail,
                    Err(err) => {
                        tracing::error!(error = %err, "following the log store failed");
                        let notice =
                            format!("Cannot read the log store: {:#}", anyhow::Error::from(err));
                        return Some((Self::closing(&notice), None));
                    }
                };
                follow.cursor = tail.through;
                more = tail.more;
                if tail.entries.is_empty() {
                    continue;
                }
                let mut entries = tail.entries;
                for entry in &entries {
                    follow.rows.learn(entry);
                }
                // In page order within the message; commit order between messages.
                entries.sort_by_key(LogEntry::key);
                let chunk = LogRecords {
                    rows: entries.iter().map(|e| follow.rows.row(e)).collect(),
                    stack_column: follow.rows.stack_column,
                    notice: None,
                };
                let event = chunk.event(Some(follow.cursor.to_string()));
                return Some((event.into_iter().collect(), Some((follow, ends, more))));
            }
        })
        .flat_map(stream::iter)
        .map(Ok)
    }
}

/// What the follow stream of a source other than the log store reads ([`SourceFollow`]).
#[async_trait::async_trait]
pub trait SourceTail: Send + 'static {
    /// Waits until the source may have entries past what was read; `false` once it never
    /// will.
    async fn changed(&mut self) -> bool;

    /// The entries after `cursor` the page's options match, in page order, at most `limit`.
    async fn after(&mut self, cursor: RecordKey, limit: u32) -> Result<SourceRead, String>;
}

/// What one read of a [`SourceTail`] found.
#[derive(Debug, Default)]
pub struct SourceRead {
    pub entries: Vec<SourceEntry>,
    /// Where to read on from at once, when the read stopped at its limit or its scan cap.
    pub more: Option<RecordKey>,
}

/// The follow stream of the newest page of a source other than the log store: what its
/// [`SourceTail`] reads after the page's newest entry, by [`RecordKey`], each time it says the
/// source changed, as [`LogFollow`] sends the log store's (the same messages, the key as the
/// event ID). It reads once at the start, for what changed before the stream was opened, and
/// ends as [`LogFollow`] does.
pub struct SourceFollow<T> {
    tail: T,
    closing: CancellationToken,
    rows: Rows,
    cursor: RecordKey,
}

impl<T: SourceTail> SourceFollow<T> {
    /// The stream of `mount` for `source` with the options of `params`, read from `tail`,
    /// after the key `last_event_id` (a reconnect), else the `from` the page gave, else now.
    pub fn open(
        reader: &LogReader,
        mount: &LogMount,
        params: &LogParams,
        source: LogSource,
        last_event_id: Option<&str>,
        tail: T,
    ) -> Result<Self, String> {
        let now = SystemTime::now();
        let plan = SourcePlan::new(params, now);
        if let Some(invalid) = plan.invalid {
            return Err(invalid);
        }
        let cursor = last_event_id
            .and_then(|id| id.trim().parse().ok())
            .or_else(|| LogParams::text(&params.from).and_then(|key| key.parse().ok()))
            .unwrap_or(RecordKey {
                time: Timestamp::from(now),
                id: 0,
            });
        Ok(Self {
            tail,
            closing: reader.closing.clone(),
            rows: Rows {
                stack_column: mount.scope.stack_column(),
                text: plan.text,
                ..Rows::new(
                    Vec::new(),
                    &[],
                    Links::of(&mount.action, &mount.scope, params, source),
                )
            },
            cursor,
        })
    }

    /// The response for `opened`, as [`LogFollow::respond`].
    pub fn respond(
        opened: Result<Self, String>,
        ends: impl Future<Output = String> + Send + 'static,
    ) -> Response {
        let events = match opened {
            Ok(follow) => follow.stream(ends).boxed(),
            Err(error) => stream::iter(LogFollow::closing(&error)).map(Ok).boxed(),
        };
        Sse::new(events)
            .keep_alive(KeepAlive::new().interval(LogFollow::KEEP_ALIVE))
            .into_response()
    }

    fn stream(
        self,
        ends: impl Future<Output = String> + Send + 'static,
    ) -> impl futures_util::Stream<Item = Result<Event, Infallible>> + Send + 'static {
        let ends: Pin<Box<dyn Future<Output = String> + Send>> = Box::pin(ends);
        // `more`: read without waiting, at the start and after a full read.
        stream::unfold(Some((self, ends, true)), |state| async move {
            let (mut follow, mut ends, mut more) = state?;
            loop {
                if !more {
                    let notice = tokio::select! {
                        changed = follow.tail.changed() => (!changed).then(|| {
                            "hoster stopped reporting changes: reload the page to follow again."
                                .to_owned()
                        }),
                        notice = &mut ends => Some(notice),
                        () = follow.closing.cancelled() => {
                            Some("hoster is shutting down: reload the page once it is back.".to_owned())
                        }
                    };
                    if let Some(notice) = notice {
                        return Some((LogFollow::closing(&notice), None));
                    }
                }
                let read = match follow.tail.after(follow.cursor, LogFollow::CHUNK).await {
                    Ok(read) => read,
                    Err(notice) => return Some((LogFollow::closing(&notice), None)),
                };
                let entries = read.entries;
                if let Some(last) = entries.last() {
                    follow.cursor = follow.cursor.max(last.key);
                }
                if let Some(on) = read.more {
                    follow.cursor = follow.cursor.max(on);
                }
                more = read.more.is_some();
                if entries.is_empty() {
                    continue;
                }
                let chunk = LogRecords {
                    rows: entries.iter().map(|e| follow.rows.source_row(e)).collect(),
                    stack_column: follow.rows.stack_column,
                    notice: None,
                };
                let event = chunk.event(Some(follow.cursor.to_string()));
                return Some((event.into_iter().collect(), Some((follow, ends, more))));
            }
        })
        .flat_map(stream::iter)
        .map(Ok)
    }
}

/// One message of a follow stream: rows appended to the record list, or the row it closes
/// with, and the partials that trim the list and remove its "no records" notice.
#[derive(Debug, Template)]
#[template(path = "log_records.html")]
struct LogRecords {
    rows: Vec<LogRow>,
    stack_column: bool,
    /// The closing row's text.
    notice: Option<String>,
}

impl LogRecords {
    fn notice(message: &str) -> Self {
        Self {
            rows: Vec::new(),
            stack_column: false,
            notice: Some(message.to_owned()),
        }
    }

    /// The `nth-last-child` argument that matches every row but the newest [`LogFollow::KEEP`].
    fn beyond(&self) -> usize {
        LogFollow::KEEP + 1
    }

    /// The current time as the closing row shows it.
    fn now(&self) -> (String, String) {
        let time = LogsView::utc(Timestamp::now());
        let shown = time[11..23].to_owned();
        (time, shown)
    }

    /// The message as an unnamed SSE event with `id`, or `None` when it cannot be rendered.
    fn event(&self, id: Option<String>) -> Option<Event> {
        let html = match self.render() {
            Ok(html) => html,
            Err(err) => {
                tracing::error!(error = %err, "rendering log records failed");
                return None;
            }
        };
        let event = Event::default().data(html);
        Some(match id {
            Some(id) => event.id(id),
            None => event,
        })
    }
}

#[cfg(test)]
mod tests;
