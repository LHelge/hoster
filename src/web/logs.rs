//! The stored logs view (ADR-0067 §4, §7, §8; ADR-0068): a page of the log store's records,
//! every service merged oldest to newest (by time, then record id), with a service column,
//! a *since* range and paging back through history, replaced and removed containers included.
//!
//! One view, two scopes ([`LogScope`]): the stack logs tab mounts it with its stack fixed
//! (`/stacks/{name}/logs`), and the Telemetry page's Logs tab mounts it across every stack. A
//! mount says where the view lives and what it knows beyond the store ([`LogMount`]: the
//! services the current revision declares, the containers whose live log it links), and
//! [`LogsView::load`] reads the store and builds what `logs_view.html` renders.
//!
//! Every option is a plain GET parameter ([`LogParams`]): `service`, `since` (a preset, a
//! duration or a UTC time; 1h by default), `level` ([`LevelFilter`]), `q` and `regex=on` (grep:
//! a substring ignoring case, or a regular expression, of the message and the fields) and the
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
//! hoster's own records that name the stack are listed under the service `hoster`, with a chip
//! of its own. A JSON line's level is shown; a text line has none (ADR-0067 §3), and a
//! `stderr` line is set apart as on the live log. The marker record for dropped lines (§7) is
//! a notice row. When the store's oldest record is newer than the start of the range (the
//! size cap, or a young server), the view says from when its data starts. When the store
//! cannot be read, it says so and the mount's live log links stay (§10).

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use std::fmt;
use std::ops::Range;
use std::str::FromStr;

use super::containers::{LogEntry as LiveLine, LogSince, SinceLink};
use super::system::LogStoreState;
use super::ui::Count;
use crate::store::{
    self, Level, LogPage, LogQuery, LogStore, PageAt, RecordKey, ServiceSeen, Stream, TextFilter,
    Timestamp,
};

/// Whose records the view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogScope {
    /// One stack's: the stack logs tab.
    Stack(String),
    /// Every stack's, or the one picked: the Telemetry page's Logs tab.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the Telemetry page's Logs tab (task ckc) mounts it"
        )
    )]
    All { stack: Option<String> },
}

impl LogScope {
    /// The store's scope for this one.
    fn store(&self) -> store::LogScope {
        match self {
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

/// The log store as the pages read it: the [`LogStoreState`] hoster opened at start.
#[derive(Debug, Clone)]
pub struct LogReader(LogStoreState);

impl From<LogStoreState> for LogReader {
    fn from(state: LogStoreState) -> Self {
        Self(state)
    }
}

impl Default for LogReader {
    /// No log store (tests that do not read logs).
    fn default() -> Self {
        Self(LogStoreState::Unavailable("it was not opened".to_owned()))
    }
}

impl LogReader {
    /// What one page shows: its records, the services of its range, where the data starts and
    /// what a level filter hides.
    async fn read(&self, plan: &Plan) -> Result<Read, String> {
        let store: &LogStore = match &self.0 {
            LogStoreState::Open(store) => store,
            LogStoreState::Unavailable(error) => {
                return Err(format!("The log store is unavailable: {error}"));
            }
        };
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
        let read = async {
            let (seen, page, oldest, unleveled) = tokio::try_join!(
                store.services_in(query.range.clone(), &query.scope),
                records,
                store.oldest_record_time(),
                unleveled,
            )?;
            Ok::<_, store::StoreError>(Read {
                seen,
                page,
                oldest,
                unleveled,
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
}

/// The store query one page runs, and what was wrong with its options.
#[derive(Debug)]
struct Plan {
    query: LogQuery,
    /// An option that did not parse and was replaced by its default.
    problem: Option<String>,
    /// Why the grep pattern does not compile; no records are read then.
    invalid: Option<String>,
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

/// The options of the view, as GET parameters. Every field is text, as an HTML form sends it.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct LogParams {
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
}

impl LogParams {
    /// The *since* presets, shared by both scopes.
    pub const SINCE_PRESETS: [&str; 6] = ["5m", "15m", "1h", "6h", "24h", "7d"];
    /// The range without a `since`.
    pub const DEFAULT_SINCE: &str = "1h";

    fn text(value: &Option<String>) -> Option<&str> {
        value.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }

    fn service(&self) -> Option<&str> {
        Self::text(&self.service)
    }

    /// The *since* in effect, as typed.
    fn since(&self) -> &str {
        Self::text(&self.since).unwrap_or(Self::DEFAULT_SINCE)
    }

    /// The start of the range at `now`; the default's when `since` does not parse, with why.
    fn start(&self, now: SystemTime) -> (Timestamp, Option<String>) {
        let default = || {
            Timestamp::from(
                Self::DEFAULT_SINCE
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

    /// The options every link keeps: all but the page cursor, and only those given.
    fn filters(&self) -> Vec<(&'static str, String)> {
        let level = self.level().0;
        [
            ("service", self.service()),
            ("since", Self::text(&self.since)),
            ("level", (level != LevelFilter::All).then(|| level.as_str())),
            ("q", self.q()),
            ("regex", self.regex().then_some("on")),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((key, value?.to_owned())))
        .collect()
    }

    /// Whether level or grep narrow the page.
    fn filtered(&self) -> bool {
        self.level().0 != LevelFilter::All || self.q().is_some()
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
#[derive(Debug)]
struct Links {
    action: String,
    filters: Vec<(&'static str, String)>,
}

impl Links {
    /// The view with `set` replacing those options (`None` drops one), and no page cursor
    /// unless `set` names one.
    fn to(&self, set: &[(&'static str, Option<&str>)]) -> String {
        let mut pairs: Vec<(&str, &str)> = self
            .filters
            .iter()
            .filter(|(key, _)| set.iter().all(|(k, _)| k != key))
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        pairs.extend(set.iter().filter_map(|(k, v)| Some((*k, (*v)?))));
        if pairs.is_empty() {
            return self.action.clone();
        }
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        format!("{}?{query}", self.action)
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
}

/// A run of a message's text, marked where grep matched.
#[derive(Debug, PartialEq, Eq)]
pub struct Span<'a> {
    pub text: &'a str,
    pub mark: bool,
}

impl LogRow {
    /// The message in runs, marked and not, for the template to escape one by one.
    pub fn spans(&self) -> Vec<Span<'_>> {
        let mut spans = Vec::new();
        let mut at = 0;
        for mark in &self.marks {
            if mark.start > at {
                spans.push(Span {
                    text: &self.message[at..mark.start],
                    mark: false,
                });
            }
            spans.push(Span {
                text: &self.message[mark.clone()],
                mark: true,
            });
            at = mark.end;
        }
        if at < self.message.len() || spans.is_empty() {
            spans.push(Span {
                text: &self.message[at..],
                mark: false,
            });
        }
        spans
    }
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
}

impl LogsView {
    /// The most records a page shows.
    pub const PAGE_SIZE: u32 = 200;
    /// The colour classes of the service chips and rows, in turn by service order (ADR-0054:
    /// classes, never inline styles).
    const PALETTE: [&str; 4] = ["svc-0", "svc-1", "svc-2", "svc-3"];
    /// hoster's own records (ADR-0068).
    const HOSTER_CLASS: &str = "svc-hoster";
    const ALL_CLASS: &str = "svc-all";
    /// The service hoster's own records are listed under.
    const HOSTER: &str = "hoster";
    /// The options the form edits rather than keeps as hidden fields.
    const EDITED: [&str; 3] = ["since", "q", "regex"];
    /// How far past now the range ends, so a line stamped as the page renders is on it.
    const END_SLACK: Duration = Duration::from_secs(1);

    /// The view of `mount` with the options of `params`, read from `reader`.
    pub async fn load(reader: &LogReader, mount: LogMount, params: &LogParams) -> Self {
        let now = SystemTime::now();
        let plan = Self::plan(&mount.scope, params, now);
        let read = reader.read(&plan).await;
        Self::new(mount, params, now, plan, read)
    }

    /// The store query `params` ask for at `now`, and what was wrong with them.
    fn plan(scope: &LogScope, params: &LogParams, now: SystemTime) -> Plan {
        let (start, since_problem) = params.start(now);
        let (page, page_problem) = params.page();
        let (level, level_problem) = params.level();
        let end = Timestamp::from(now) + Self::END_SLACK;
        let mut query = LogQuery::new(start..end.max(start), scope.store());
        query.services = params.service().map(str::to_owned).into_iter().collect();
        query.levels = level.levels();
        let invalid = match params.grep() {
            Ok(text) => {
                query.text = text;
                None
            }
            Err(message) => Some(message),
        };
        query.page = page;
        query.limit = Self::PAGE_SIZE;
        Plan {
            query,
            problem: since_problem.or(page_problem).or(level_problem),
            invalid,
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
        } = plan;
        let links = Links {
            action: mount.action.clone(),
            filters: params.filters(),
        };
        let (read, unreadable) = match read {
            Ok(read) => (read, None),
            Err(error) => (Read::default(), Some(error)),
        };
        let services = Self::services(&mount.services, &read.seen);
        let class = |service: &str| {
            if service == Self::HOSTER {
                return Self::HOSTER_CLASS;
            }
            let n = services.iter().position(|s| s == service).unwrap_or(0);
            Self::PALETTE[n % Self::PALETTE.len()]
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
            class: class(service),
        }));

        let generations: HashMap<(Option<&str>, &str), usize> = read
            .seen
            .iter()
            .map(|s| {
                (
                    (s.stack.as_deref(), s.service.as_str()),
                    s.generations.len(),
                )
            })
            .collect();
        let stack_column = mount.scope.stack_column();
        let today = Self::utc(now.into())[..10].to_owned();
        let all_today = read
            .page
            .entries
            .iter()
            .all(|e| Self::utc(e.record.time).starts_with(&today));
        let rows: Vec<LogRow> = read
            .page
            .entries
            .iter()
            .map(|entry| {
                let record = &entry.record;
                let message = LiveLine::printable(&record.message);
                let time = Self::utc(record.time);
                let shown = if all_today {
                    time[11..23].to_owned()
                } else {
                    time[5..23].replacen('T', " ", 1)
                };
                let several = generations
                    .get(&(record.stack.as_deref(), record.service.as_str()))
                    .is_some_and(|n| *n > 1);
                LogRow {
                    shown,
                    time,
                    stack: stack_column
                        .then(|| record.stack.clone().unwrap_or_else(|| "—".to_owned())),
                    service: record.service.clone(),
                    generation: record.generation.filter(|_| several),
                    class: class(&record.service),
                    stderr: record.stream == Stream::Stderr,
                    level: record.level,
                    marks: match (&query.text, record.marker) {
                        (Some(text), false) => text.find_all(&message),
                        _ => Vec::new(),
                    },
                    message,
                    cut: record.cut,
                    marker: record.marker,
                }
            })
            .collect();

        let page = &read.page;
        let first = page.entries.first().map(|e| e.key().to_string());
        let last = page.entries.last().map(|e| e.key().to_string());
        let reading_older = !matches!(query.page, PageAt::After(_));
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
        let newer = last
            .filter(|_| page.newer && !(stopped.is_some() && !reading_older))
            .map(|key| links.to(&[("after", Some(&key))]));
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
        let empty = rows.is_empty() && !paged && unreadable.is_none() && invalid.is_none();
        let cleared = (empty && params.filtered()).then(|| {
            let mut active = Vec::new();
            if level != LevelFilter::All {
                active.push(format!("level {level}"));
            }
            if let Some(q) = params.q() {
                let kind = if params.regex() { "regex" } else { "grep" };
                active.push(format!("{kind} “{q}”"));
            }
            (
                active.join(" and "),
                links.to(&[("level", None), ("q", None), ("regex", None)]),
            )
        });
        let starts_at = read
            .oldest
            .filter(|oldest| *oldest > query.range.start)
            .map(|oldest| humantime::format_rfc3339_seconds(oldest.into()).to_string());
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
        Self {
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
            unleveled,
            stopped,
            live: mount.live,
            stack_column,
            empty,
            cleared,
            rows,
            older,
            newer,
            newest: paged.then(|| links.to(&[])),
            starts_at,
            problem,
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

#[cfg(test)]
mod tests;
