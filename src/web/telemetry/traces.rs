//! The Telemetry page's Traces tab (`/telemetry/traces`, ADR-0069, epic qnh): the trace list
//! over a range, one row per trace from its root (`LogStore::traces`), with the page's stack
//! selector and range presets, and the trace picked by `?trace=<id>` below it.
//!
//! Every option is a GET parameter ([`TraceParams`]): `stack`, `since` (a preset from 5m to
//! 24h, or anything the Logs tab takes: spans are kept 3 days), `q` (grep on span names; no
//! query language, OQ-43), `sort`, `status`, `slower` (the root's minimum duration), `page`
//! (from 1), `trace` and `span` (a span of that trace, selected in its waterfall). A link changes
//! some and keeps the rest; a new filter starts at the first page, picking a trace keeps the page
//! and drops the span.
//!
//! Services are coloured by class from the categorical palette (`docs/design/README.md`):
//! hoster's proxy its own, the others in turn by name over everything the page shows, so a
//! service has one colour in every row and in the trace below. Bars are server-drawn SVG
//! ([`Meter`]) and tones are classes: nothing is an inline style (ADR-0054). The trace picked is
//! drawn as a [`Waterfall`] (`traces/waterfall.rs`).

use std::ops::Range;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::super::containers::{LogSince, SinceLink};
use super::super::logs::{LogScope, LogsView, StackOption};
use super::super::ui::{Count, Meter, Tone};
use crate::stats::Elapsed;
use crate::store::{
    LogStore, Retention, Span, SpanId, StoreError, Timestamp, Trace, TraceId, TracePage,
    TraceQuery, TraceSort, TraceStatus, TraceSummary,
};

pub use self::waterfall::Waterfall;

mod waterfall;

/// The options of the Traces tab, as GET parameters; every field is text, as an HTML form
/// sends it. Of a parameter given twice, the first counts.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(from = "Vec<(String, String)>")]
pub struct TraceParams {
    /// The stack picked; every stack when absent or empty ([`LogScope::All`]).
    stack: Option<String>,
    /// Where the range starts: a preset, a duration ago or a UTC time.
    since: Option<String>,
    /// Grep on span names.
    q: Option<String>,
    /// A [`TraceSort`]; newest first when absent.
    sort: Option<String>,
    /// A [`TraceStatus`]; every trace when absent.
    status: Option<String>,
    /// The root's minimum duration, e.g. `500ms`.
    slower: Option<String>,
    /// The page of the list, from 1.
    page: Option<String>,
    /// The trace picked, shown below the list.
    trace: Option<String>,
    /// The span of the trace picked that is selected in its waterfall.
    span: Option<String>,
}

impl From<Vec<(String, String)>> for TraceParams {
    fn from(pairs: Vec<(String, String)>) -> Self {
        let mut params = Self::default();
        for (key, value) in pairs {
            let slot = match key.as_str() {
                "stack" => &mut params.stack,
                "since" => &mut params.since,
                "q" => &mut params.q,
                "sort" => &mut params.sort,
                "status" => &mut params.status,
                "slower" => &mut params.slower,
                "page" => &mut params.page,
                "trace" => &mut params.trace,
                "span" => &mut params.span,
                _ => continue,
            };
            slot.get_or_insert(value);
        }
        params
    }
}

impl TraceParams {
    /// The range presets: spans are kept 3 days, so 7d is not one.
    pub const SINCE_PRESETS: [&str; 5] = ["5m", "15m", "1h", "6h", "24h"];
    /// The range without a `since`, as on the Logs tab across stacks.
    pub const DEFAULT_SINCE: &str = "15m";
    /// How far past now the range ends, so a trace that just started is in it.
    const END_SLACK: Duration = Duration::from_secs(1);

    fn text(value: &Option<String>) -> Option<&str> {
        value.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }

    /// The stack picked, when given.
    pub fn stack(&self) -> Option<&str> {
        Self::text(&self.stack)
    }

    /// The *since* given, when given: what the Logs tab's link keeps.
    pub fn since_given(&self) -> Option<&str> {
        Self::text(&self.since)
    }

    /// The *since* in effect, as typed.
    fn since(&self) -> &str {
        Self::text(&self.since).unwrap_or(Self::DEFAULT_SINCE)
    }

    /// The scope of the stack selector: every stack, or the one picked.
    pub fn scope(&self) -> LogScope {
        LogScope::All {
            stack: self.stack().map(str::to_owned),
        }
    }

    /// The range at `now`; the default's when `since` does not parse, with why.
    pub fn range(&self, now: SystemTime) -> (Range<Timestamp>, Option<String>) {
        let (since, problem) = match self.since().parse::<LogSince>() {
            Ok(since) => (since, None),
            Err(message) => (
                Self::DEFAULT_SINCE.parse().expect("the default parses"),
                Some(message),
            ),
        };
        let start = Timestamp::from(since.resolve(now));
        let end = Timestamp::from(now) + Self::END_SLACK;
        (start..end.max(start), problem)
    }

    fn sort(&self) -> (TraceSort, Option<String>) {
        match Self::text(&self.sort).map(|s| (s, s.parse::<TraceSort>())) {
            None => (TraceSort::default(), None),
            Some((_, Ok(sort))) => (sort, None),
            Some((given, Err(_))) => (
                TraceSort::default(),
                Some(format!(
                    "`{given}` is not a sort: use newest, slowest or errors; showing the newest first."
                )),
            ),
        }
    }

    fn status(&self) -> (Option<TraceStatus>, Option<String>) {
        match Self::text(&self.status).map(|s| (s, s.parse::<TraceStatus>())) {
            None => (None, None),
            Some((_, Ok(status))) => (Some(status), None),
            Some((given, Err(_))) => (
                None,
                Some(format!(
                    "`{given}` is not a trace status: use ok or error; showing every status."
                )),
            ),
        }
    }

    fn slower(&self) -> (Option<Duration>, Option<String>) {
        match Self::text(&self.slower).map(|s| (s, humantime::parse_duration(s))) {
            None => (None, None),
            Some((_, Ok(min))) => (Some(min), None),
            Some((given, Err(_))) => (
                None,
                Some(format!(
                    "`{given}` is not a duration such as 500ms; showing traces of any duration."
                )),
            ),
        }
    }

    /// The page from 0; the first when `page` is not a page number, with why.
    fn page(&self) -> (u32, Option<String>) {
        match Self::text(&self.page).map(|p| (p, p.parse::<u32>())) {
            None => (0, None),
            Some((_, Ok(page))) if page >= 1 => (page - 1, None),
            Some((given, _)) => (
                0,
                Some(format!(
                    "`{given}` is not a page number; showing the first page."
                )),
            ),
        }
    }

    fn q(&self) -> Option<&str> {
        self.q.as_deref().filter(|q| !q.trim().is_empty())
    }

    /// The trace picked; none when `trace` is not a trace id, with why.
    fn trace(&self) -> (Option<TraceId>, Option<String>) {
        match Self::text(&self.trace).map(|t| (t, t.parse::<TraceId>())) {
            None => (None, None),
            Some((_, Ok(id))) => (Some(id), None),
            Some((given, Err(_))) => (
                None,
                Some(format!(
                    "`{given}` is not a trace id: one is 32 hexadecimal digits."
                )),
            ),
        }
    }

    /// The span selected; none when `span` is not a span id, with why.
    fn span(&self) -> (Option<SpanId>, Option<String>) {
        match Self::text(&self.span).map(|s| (s, s.parse::<SpanId>())) {
            None => (None, None),
            Some((_, Ok(id))) => (Some(id), None),
            Some((given, Err(_))) => (
                None,
                Some(format!(
                    "`{given}` is not a span id: one is 16 hexadecimal digits."
                )),
            ),
        }
    }

    /// The options in effect as they are written, in a fixed order, for links: those given
    /// and valid, without the page.
    fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        let mut keep = |key, value: Option<String>| pairs.extend(value.map(|v| (key, v)));
        keep("stack", self.stack().map(str::to_owned));
        keep("since", Self::text(&self.since).map(str::to_owned));
        keep("q", self.q().map(str::to_owned));
        let sort = Self::text(&self.sort).and_then(|s| s.parse::<TraceSort>().ok());
        keep("sort", sort.map(|s| s.to_string()));
        keep("status", self.status().0.map(|s| s.to_string()));
        keep(
            "slower",
            Self::text(&self.slower)
                .filter(|_| self.slower().0.is_some())
                .map(str::to_owned),
        );
        let trace = self.trace().0;
        keep("trace", trace.map(|id| id.to_string()));
        let span = trace.and(self.span().0);
        keep("span", span.map(|id| id.to_string()));
        pairs
    }
}

/// The URLs of the tab: its options, with some changed.
#[derive(Debug, Clone)]
struct Links {
    action: &'static str,
    pairs: Vec<(&'static str, String)>,
    /// The page shown, from 0.
    page: u32,
}

impl Links {
    /// The tab with `set` replacing those options (`None` drops one), on the first page.
    fn to(&self, set: &[(&'static str, Option<&str>)]) -> String {
        let mut pairs: Vec<(&str, &str)> = self
            .pairs
            .iter()
            .filter(|(key, _)| set.iter().all(|(k, _)| k != key))
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        pairs.extend(set.iter().filter_map(|(k, v)| Some((*k, (*v)?))));
        Self::href(self.action, pairs)
    }

    /// The same list on page `page` (from 0).
    fn at_page(&self, page: u32) -> String {
        let number = (page + 1).to_string();
        let mut pairs: Vec<(&str, &str)> =
            self.pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        if page > 0 {
            let at = pairs
                .iter()
                .position(|(k, _)| *k == "trace")
                .unwrap_or(pairs.len());
            pairs.insert(at, ("page", &number));
        }
        Self::href(self.action, pairs)
    }

    /// The same list and page with `trace` picked, or none, and no span selected.
    fn picking(&self, trace: Option<TraceId>) -> String {
        self.with("span", None)
            .with("trace", trace.map(|id| id.to_string()))
            .at_page(self.page)
    }

    /// The same list, page and trace with `span` selected, or none.
    fn selecting(&self, span: Option<SpanId>) -> String {
        self.with("span", span.map(|id| id.to_string()))
            .at_page(self.page)
    }

    /// These links with `key` set to `value` (last), or dropped.
    fn with(&self, key: &'static str, value: Option<String>) -> Self {
        Self {
            pairs: self
                .pairs
                .iter()
                .filter(|(k, _)| *k != key)
                .cloned()
                .chain(value.map(|v| (key, v)))
                .collect(),
            ..self.clone()
        }
    }

    fn href(action: &str, pairs: Vec<(&str, &str)>) -> String {
        if pairs.is_empty() {
            return action.to_owned();
        }
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        format!("{action}?{query}")
    }
}

/// The colour class of each service the page shows: hoster's proxy its own, the others in
/// turn by name, so a service is the same colour in every row and in the trace below.
#[derive(Debug, Clone, Default)]
pub struct ServicePalette(Vec<String>);

impl ServicePalette {
    /// The palette of `services`, in any order and with repeats.
    fn of<'a>(services: impl IntoIterator<Item = &'a str>) -> Self {
        let mut names: Vec<String> = services
            .into_iter()
            .filter(|s| *s != Span::PROXY_SERVICE)
            .map(str::to_owned)
            .collect();
        names.sort();
        names.dedup();
        Self(names)
    }

    /// The class of `service`: `svc-hoster` for the proxy, `svc-0`… for the others.
    pub fn class(&self, service: &str) -> &'static str {
        if service == Span::PROXY_SERVICE {
            return LogsView::HOSTER_CLASS;
        }
        let n = self.0.iter().position(|s| s == service).unwrap_or(0);
        LogsView::PALETTE[n % LogsView::PALETTE.len()]
    }
}

/// A service of a row: its name and colour class.
#[derive(Debug, PartialEq, Eq)]
pub struct ServiceChip {
    pub name: String,
    pub class: &'static str,
}

/// An option of a select, the one in effect marked.
#[derive(Debug, PartialEq, Eq)]
pub struct Choice {
    pub value: &'static str,
    pub label: &'static str,
    pub current: bool,
}

/// A link among others, the one in effect marked; with a dot of `class`.
#[derive(Debug, PartialEq, Eq)]
pub struct Chip {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
    pub class: &'static str,
}

/// One row of the trace list.
#[derive(Debug)]
pub struct TraceRow {
    /// The tab with this trace picked.
    pub href: String,
    /// The trace picked.
    pub current: bool,
    /// RFC 3339 in UTC, to the millisecond.
    pub time: String,
    /// The time of day, with the date when the list spans more than today.
    pub shown: String,
    /// The root span's name.
    pub name: String,
    pub stack: String,
    pub services: Vec<ServiceChip>,
    /// Only the proxy's spans: "app not instrumented".
    pub proxy_only: bool,
    pub spans: Count,
    pub duration: String,
    /// The duration against the slowest listed.
    pub bar: Meter,
    pub status: TraceStatus,
}

impl TraceRow {
    /// The class of the status cell: its tone.
    pub fn status_class(&self) -> &'static str {
        match self.status {
            TraceStatus::Ok => "trace-ok",
            TraceStatus::Error => "trace-error",
        }
    }
}

/// The trace `?trace=` asks for, and the store's trace of that id if it has one.
type Requested = (TraceId, Option<Trace>);

/// The trace `?trace=` picks.
#[derive(Debug)]
pub enum Picked {
    /// Found, as its waterfall.
    Found(Box<Waterfall>),
    /// No span and no request row of it is left: never sent, or older than spans are kept.
    Missing(TraceId),
}

impl Picked {
    pub fn id(&self) -> TraceId {
        match self {
            Picked::Found(waterfall) => waterfall.id,
            Picked::Missing(id) => *id,
        }
    }

    /// The days spans are kept, which a missing trace may be older than.
    pub fn kept_days(&self) -> u64 {
        Retention::SPAN_MAX_AGE.as_secs() / 86_400
    }
}

/// The Traces tab under the page's header: toolbar, list and the trace picked.
#[derive(Debug)]
pub struct TracesView {
    pub action: &'static str,
    /// The options the form keeps as hidden fields (those it does not edit).
    pub keep: Vec<(&'static str, String)>,
    pub stacks: Vec<StackOption>,
    pub presets: Vec<SinceLink>,
    /// The grep field as typed.
    pub q: String,
    /// The minimum duration field as typed.
    pub slower: String,
    pub sorts: Vec<Choice>,
    pub statuses: Vec<Chip>,
    pub rows: Vec<TraceRow>,
    /// `9 of 18,204 · newest first`.
    pub heading: String,
    /// The traces the range and stack hold: the tab's count; `None` when unread.
    pub total: Option<u64>,
    pub previous: Option<String>,
    pub next: Option<String>,
    /// The filters in effect for an empty list, and the list without them.
    pub cleared: Option<(String, String)>,
    pub picked: Option<Picked>,
    /// Closes the trace picked.
    pub close: String,
    /// An option that did not parse and was replaced by its default.
    pub problem: Option<String>,
    /// Why the store could not be read.
    pub unreadable: Option<String>,
}

impl TracesView {
    /// The tab's path.
    pub const PATH: &str = "/telemetry/traces";
    /// The options the form edits; the others are kept as hidden fields.
    const EDITED: [&str; 5] = ["stack", "q", "sort", "status", "slower"];

    /// The tab for `params` at `now`, read from `store` (or why it could not be), with the
    /// stacks of the current state in its selector.
    pub async fn load(
        store: Result<&LogStore, String>,
        params: &TraceParams,
        known: &[String],
        now: SystemTime,
    ) -> Self {
        let (query, problem) = Self::query(params, now);
        let (picked_id, trace_problem) = params.trace();
        let span_problem = picked_id.and(params.span().1);
        let read = match store {
            Ok(store) => {
                let picked = async {
                    match picked_id {
                        Some(id) => store.load_trace(id).await.map(|t| Some((id, t))),
                        None => Ok(None),
                    }
                };
                tokio::try_join!(store.traces(&query), picked).map_err(|err: StoreError| {
                    tracing::error!(error = %err, "reading the trace list failed");
                    format!("Cannot read the log store: {:#}", anyhow::Error::from(err))
                })
            }
            Err(error) => Err(error),
        };
        let problem = problem.or(trace_problem).or(span_problem);
        Self::new(params, query, read, known, problem, now)
    }

    /// The store query `params` ask for at `now`, and what was wrong with them.
    fn query(params: &TraceParams, now: SystemTime) -> (TraceQuery, Option<String>) {
        let (range, since_problem) = params.range(now);
        let (sort, sort_problem) = params.sort();
        let (status, status_problem) = params.status();
        let (min_duration, slower_problem) = params.slower();
        let (page, page_problem) = params.page();
        let mut query = TraceQuery::new(range, params.scope().store());
        query.sort = sort;
        query.status = status;
        query.min_duration = min_duration;
        query.grep = params.q().map(str::to_owned);
        query.page = page;
        let problem = since_problem
            .or(sort_problem)
            .or(status_problem)
            .or(slower_problem)
            .or(page_problem);
        (query, problem)
    }

    fn new(
        params: &TraceParams,
        query: TraceQuery,
        read: Result<(TracePage, Option<Requested>), String>,
        known: &[String],
        problem: Option<String>,
        now: SystemTime,
    ) -> Self {
        let links = Links {
            action: Self::PATH,
            pairs: params.pairs(),
            page: query.page,
        };
        let (page, picked, unreadable, total) = match read {
            Ok((page, picked)) => {
                let total = Some(page.total);
                (page, picked, None, total)
            }
            Err(error) => (TracePage::default(), None, Some(error), None),
        };
        let picked_id = picked.as_ref().map(|(id, _)| *id);
        let mut services: Vec<&str> = page
            .traces
            .iter()
            .flat_map(|t| t.services.iter().map(String::as_str))
            .collect();
        if let Some((_, Some(trace))) = &picked {
            services.extend(trace.services());
        }
        // The picked trace's services too, so its waterfall colours them alike.
        let palette = ServicePalette::of(services);
        let picked = picked.map(|(id, trace)| match trace {
            Some(trace) => Picked::Found(Box::new(Waterfall::new(
                &trace,
                &palette,
                &links,
                params.span().0,
            ))),
            None => Picked::Missing(id),
        });
        let slowest = page
            .traces
            .iter()
            .map(|t| t.duration)
            .max()
            .unwrap_or_default();
        let today = Self::utc(now.into())[..10].to_owned();
        let dated = !page
            .traces
            .iter()
            .all(|t| Self::utc(t.start.into()).starts_with(&today));
        let rows = page
            .traces
            .iter()
            .map(|t| Self::row(t, &links, &palette, picked_id, slowest, dated))
            .collect();
        let shown = page.traces.len() as u64;
        let heading = format!(
            "{} of {} · {}",
            Count(shown),
            Count(page.matched),
            Self::sort_label(query.sort)
        );
        let previous =
            (query.page > 0 && unreadable.is_none()).then(|| links.at_page(query.page - 1));
        let next = page.more.then(|| links.at_page(query.page + 1));
        let cleared = (page.traces.is_empty() && unreadable.is_none() && query.page == 0)
            .then(|| Self::cleared(&query, params, &links))
            .flatten();
        let presets = TraceParams::SINCE_PRESETS
            .into_iter()
            .map(|label| SinceLink {
                label,
                href: links.to(&[("since", Some(label))]),
                current: params.since() == label,
            })
            .collect();
        let statuses = [
            ("all", None, ""),
            ("ok", Some(TraceStatus::Ok), "dot-ok"),
            ("error", Some(TraceStatus::Error), "dot-fail"),
        ]
        .into_iter()
        .map(|(label, status, class)| Chip {
            label,
            href: links.to(&[("status", status.map(TraceStatus::as_str))]),
            current: query.status == status,
            class,
        })
        .collect();
        let sorts = TraceSort::ALL
            .into_iter()
            .map(|sort| Choice {
                value: sort.as_str(),
                label: Self::sort_label(sort),
                current: sort == query.sort,
            })
            .collect();
        let mut stacks = LogsView::stacks(&params.scope(), known, &[]);
        // hoster's own records have no traces: the proxy's are its routes' stacks'.
        stacks.retain(|option| option.value != LogScope::HOSTER);
        Self {
            action: Self::PATH,
            keep: links
                .pairs
                .iter()
                .filter(|(key, _)| !Self::EDITED.contains(key))
                .cloned()
                .collect(),
            stacks,
            presets,
            q: params.q.clone().unwrap_or_default(),
            slower: TraceParams::text(&params.slower)
                .unwrap_or_default()
                .to_owned(),
            sorts,
            statuses,
            rows,
            heading,
            total,
            previous,
            next,
            cleared,
            close: links.picking(None),
            picked,
            problem,
            unreadable,
        }
    }

    fn row(
        trace: &TraceSummary,
        links: &Links,
        palette: &ServicePalette,
        picked: Option<TraceId>,
        slowest: Duration,
        dated: bool,
    ) -> TraceRow {
        let time = Self::utc(trace.start.into());
        let shown = if dated {
            time[5..19].replacen('T', " ", 1)
        } else {
            time[11..19].to_owned()
        };
        let duration = Elapsed(trace.duration).to_string();
        let tone = match trace.status {
            TraceStatus::Ok => Tone::Neutral,
            TraceStatus::Error => Tone::Fail,
        };
        let max = slowest.as_secs_f64();
        // A sliver at least, so the shortest are seen.
        let value = trace.duration.as_secs_f64().max(max * 0.02);
        TraceRow {
            href: links.picking(Some(trace.trace_id)),
            current: picked == Some(trace.trace_id),
            time,
            shown,
            name: trace.name.clone(),
            stack: trace.stack.clone(),
            services: trace
                .services
                .iter()
                .map(|name| ServiceChip {
                    name: name.clone(),
                    class: palette.class(name),
                })
                .collect(),
            proxy_only: trace.proxy_only,
            spans: Count(trace.spans),
            bar: Meter::new(
                value,
                max,
                tone,
                format!("{duration} of the slowest listed"),
            ),
            duration,
            status: trace.status,
        }
    }

    /// The filters of `query` that leave the list empty, and the list without them; `None`
    /// when none are set.
    fn cleared(
        query: &TraceQuery,
        params: &TraceParams,
        links: &Links,
    ) -> Option<(String, String)> {
        let mut active = Vec::new();
        if let Some(q) = params.q() {
            active.push(format!("grep “{q}”"));
        }
        if let Some(status) = query.status {
            active.push(format!("status {status}"));
        }
        if query.min_duration.is_some() {
            let given = TraceParams::text(&params.slower).unwrap_or_default();
            active.push(format!("slower than {given}"));
        }
        (!active.is_empty()).then(|| {
            (
                active.join(", "),
                links.to(&[("q", None), ("status", None), ("slower", None)]),
            )
        })
    }

    fn sort_label(sort: TraceSort) -> &'static str {
        match sort {
            TraceSort::Newest => "newest first",
            TraceSort::Slowest => "slowest first",
            TraceSort::ErrorsFirst => "errors first",
        }
    }

    /// `time` in RFC 3339, UTC, to the millisecond.
    fn utc(time: Timestamp) -> String {
        humantime::format_rfc3339_millis(time.into()).to_string()
    }
}
