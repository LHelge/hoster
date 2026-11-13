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
//! duration or a UTC time; 1h by default) and the page cursor `before`/`after`, a record's
//! `<time>.<id>` key. Every link keeps the other options, and the form keeps them as hidden
//! fields. A page holds at most [`LogsView::PAGE_SIZE`] records; "older" and "newer" page by
//! record key, never by offset, so equal times page correctly.
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

use super::containers::{LogEntry as LiveLine, LogSince, SinceLink};
use super::system::LogStoreState;
use crate::store::{
    self, Level, LogPage, LogQuery, LogStore, PageAt, RecordKey, ServiceSeen, Stream, Timestamp,
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
    /// What one page shows: its records, the services of its range and where the data starts.
    async fn read(&self, scope: &store::LogScope, query: &LogQuery) -> Result<Read, String> {
        let store: &LogStore = match &self.0 {
            LogStoreState::Open(store) => store,
            LogStoreState::Unavailable(error) => {
                return Err(format!("The log store is unavailable: {error}"));
            }
        };
        let read = async {
            let (seen, page, oldest) = tokio::try_join!(
                store.services_in(query.range.clone(), scope),
                store.records(query),
                store.oldest_record_time(),
            )?;
            Ok::<_, store::StoreError>(Read { seen, page, oldest })
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

    /// The options every link keeps: all but the page cursor, and only those given.
    fn filters(&self) -> Vec<(&'static str, String)> {
        [
            ("service", self.service()),
            ("since", Self::text(&self.since)),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((key, value?.to_owned())))
        .collect()
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

/// A service chip of the toolbar, or "all".
#[derive(Debug)]
pub struct ServiceChip {
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
    /// The line was cut at the store's length cap.
    pub cut: bool,
    /// A notice about lines hoster lost, not a log line.
    pub marker: bool,
}

/// What `logs_view.html` renders.
#[derive(Debug)]
pub struct LogsView {
    pub action: String,
    /// The options the form keeps as hidden fields (all but `since`, which it edits).
    pub keep: Vec<(&'static str, String)>,
    /// The *since* field as typed (empty for the default).
    pub since: String,
    pub presets: Vec<SinceLink>,
    pub chips: Vec<ServiceChip>,
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
    /// How far past now the range ends, so a line stamped as the page renders is on it.
    const END_SLACK: Duration = Duration::from_secs(1);

    /// The view of `mount` with the options of `params`, read from `reader`.
    pub async fn load(reader: &LogReader, mount: LogMount, params: &LogParams) -> Self {
        let now = SystemTime::now();
        let (query, problem) = Self::query(&mount.scope, params, now);
        let read = reader.read(&mount.scope.store(), &query).await;
        Self::new(mount, params, now, &query, read, problem)
    }

    /// The store query `params` ask for at `now`, and what was wrong with them.
    fn query(scope: &LogScope, params: &LogParams, now: SystemTime) -> (LogQuery, Option<String>) {
        let (start, since_problem) = params.start(now);
        let (page, page_problem) = params.page();
        let end = Timestamp::from(now) + Self::END_SLACK;
        let mut query = LogQuery::new(start..end.max(start), scope.store());
        query.services = params.service().map(str::to_owned).into_iter().collect();
        query.page = page;
        query.limit = Self::PAGE_SIZE;
        (query, since_problem.or(page_problem))
    }

    fn new(
        mount: LogMount,
        params: &LogParams,
        now: SystemTime,
        query: &LogQuery,
        read: Result<Read, String>,
        problem: Option<String>,
    ) -> Self {
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
        let mut chips = vec![ServiceChip {
            label: "all".to_owned(),
            href: links.to(&[("service", None)]),
            current: params.service().is_none(),
            class: Self::ALL_CLASS,
        }];
        chips.extend(services.iter().map(|service| ServiceChip {
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
                    message: LiveLine::printable(&record.message),
                    cut: record.cut,
                    marker: record.marker,
                }
            })
            .collect();

        let page = &read.page;
        let first = page.entries.first().map(|e| e.key().to_string());
        let last = page.entries.last().map(|e| e.key().to_string());
        let older = first
            .filter(|_| page.older)
            .map(|key| links.to(&[("before", Some(&key))]));
        let newer = last
            .filter(|_| page.newer)
            .map(|key| links.to(&[("after", Some(&key))]));
        let paged = query.page != PageAt::Newest;
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
                .filter(|(key, _)| *key != "since")
                .cloned()
                .collect(),
            action: mount.action,
            since,
            presets,
            chips,
            live: mount.live,
            stack_column,
            empty: rows.is_empty() && !paged && unreadable.is_none(),
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
}

#[cfg(test)]
mod tests;
