//! The Telemetry page's Traces tab (ADR-0069 §6, §9; epic qnh): the trace list over a range,
//! with the selected trace below it.
//!
//! Every option is a GET parameter ([`TraceParams`]): `stack` (the page's stack selector),
//! `since` (one of [`TraceParams::RANGES`], 15m by default; spans are kept 3 days, so the
//! Logs tab's 7d is not offered), `q` (grep on span names: the root's or any stored span's,
//! OQ-43), `status` ([`TraceStatus`]), `slower` (a minimum root duration such as `500ms`),
//! `sort` ([`TraceSort`]), `page` (from 0), `trace` (the selected trace) and `span` (the
//! selected span of it: without it the trace opens on [`Trace::default_span`], and
//! [`TraceParams::NO_SPAN`] is the details closed). Every link is
//! built by [`TraceParams::href`] or [`TraceParams::select`], so it keeps the others: a filter
//! or range drops the page, selecting a trace drops what was selected in the last one
//! ([`TraceParams::SELECTION`]).
//!
//! [`TracesView::load`] reads one page of the list ([`LogStore::traces`], once per load), the
//! Logs tab's count for the same range and stack, and the selected trace, at once. A row names
//! its root span, stack, services as chips in the categorical palette ([`ServicePalette`];
//! the proxy's spans are the service [`Span::EDGE_SERVICE`] in its own colour), span count,
//! duration with a bar scaled to the slowest listed, and status. "app not instrumented" marks a
//! request whose app was called but exported nothing: the proxy's spans only, and more than its
//! server span (a request hoster answered itself has one span and no such label).
//!
//! The selected trace ([`Selection`]) is read whether or not the list shows it, so a link from
//! a log record opens it on any range; one that no longer exists says that spans are kept
//! [`Retention::SPAN_MAX_AGE`]. A found one is laid out as a [`Waterfall`], with the details of
//! its selected span under that span's row, whose log records are read once the span is known.

mod waterfall;

use std::time::{Duration, SystemTime};

use serde::Deserialize;

use super::super::logs::{Chip, LogReader, LogScope, LogsView, StackOption, Total};
use super::super::ui::{Count, Meter, Tone};
use crate::stats::Elapsed;
use crate::store::{
    self, LogStore, Retention, Span, SpanId, StoreError, Timestamp, Trace, TraceId, TraceList,
    TraceQuery, TraceSort, TraceStatus, TraceSummary,
};

use self::waterfall::Waterfall;

/// The Traces tab's options, as GET parameters: each given one, in [`TraceParams::KEYS`]
/// order. Values are trimmed (but for `q`, whose spaces are searched for) and empty ones are
/// absent; of a parameter given twice, the first counts. A later option (a span, a collapsed
/// set) is one more key here.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "Vec<(String, String)>")]
pub struct TraceParams {
    given: Vec<(&'static str, String)>,
}

impl From<Vec<(String, String)>> for TraceParams {
    fn from(pairs: Vec<(String, String)>) -> Self {
        let mut given: Vec<(&'static str, String)> = Vec::new();
        for (key, value) in pairs {
            let Some(&key) = Self::KEYS.iter().find(|k| **k == key) else {
                continue;
            };
            let value = if key == "q" { &value } else { value.trim() };
            if value.is_empty() || given.iter().any(|(k, _)| *k == key) {
                continue;
            }
            given.push((key, value.to_owned()));
        }
        given.sort_by_key(|(key, _)| Self::rank(key));
        Self { given }
    }
}

impl TraceParams {
    /// Where the tab is.
    pub const PATH: &str = "/telemetry/traces";
    /// Every option, in the order links write them.
    pub const KEYS: [&str; 9] = [
        "stack", "since", "q", "status", "slower", "sort", "page", "trace", "span",
    ];
    /// What is selected below the list; selecting another trace drops them all.
    pub const SELECTION: [&str; 2] = ["trace", "span"];
    /// The `span` of a trace whose span details were closed. Without `span` a trace opens on
    /// its default span, so closing needs a value of its own; like any value that is not a
    /// span id, it selects none.
    pub const NO_SPAN: &str = "none";
    /// The options the toolbar's form edits rather than keeps as hidden fields.
    const FORM: [&str; 3] = ["stack", "q", "slower"];
    /// The range presets: spans are kept 3 days, so nothing longer than a day.
    pub const RANGES: [(&str, Duration); 5] = [
        ("5m", Duration::from_secs(5 * 60)),
        ("15m", Duration::from_secs(15 * 60)),
        ("1h", Duration::from_secs(60 * 60)),
        ("6h", Duration::from_secs(6 * 60 * 60)),
        ("24h", Duration::from_secs(24 * 60 * 60)),
    ];
    /// The range without a `since`.
    pub const DEFAULT_RANGE: &str = "15m";

    fn rank(key: &str) -> usize {
        Self::KEYS
            .iter()
            .position(|k| *k == key)
            .unwrap_or(Self::KEYS.len())
    }

    /// The option `key`, when given.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.given
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The tab with `set` replacing those options (`None` drops one), the others kept and the
    /// page dropped unless `set` names it.
    pub fn href(&self, set: &[(&'static str, Option<&str>)]) -> String {
        let mut pairs: Vec<(&str, &str)> = self
            .given
            .iter()
            .filter(|(key, _)| *key != "page" && set.iter().all(|(k, _)| k != key))
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        pairs.extend(set.iter().filter_map(|(k, v)| Some((*k, (*v)?))));
        pairs.sort_by_key(|(key, _)| Self::rank(key));
        if pairs.is_empty() {
            return Self::PATH.to_owned();
        }
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        format!("{}?{query}", Self::PATH)
    }

    /// The tab with trace `id` selected: the list as it is, page included, and nothing else
    /// of the last selection.
    pub fn select(&self, id: TraceId) -> String {
        let id = id.to_string();
        let page = self.get("page");
        // `trace` is in the selection: dropped, then set.
        let mut set: Vec<(&'static str, Option<&str>)> =
            Self::SELECTION.iter().map(|key| (*key, None)).collect();
        set.push(("page", page));
        set.push(("trace", Some(&id)));
        self.href(&set)
    }

    /// The tab with span `id` of the selected trace selected: everything else kept, the page
    /// included.
    pub fn select_span(&self, id: SpanId) -> String {
        let id = id.to_string();
        self.href(&[("page", self.get("page")), ("span", Some(&id))])
    }

    /// The selected span, when `span` is a span id; one that is not selects none.
    pub fn span(&self) -> Option<SpanId> {
        self.get("span")?.to_ascii_lowercase().parse().ok()
    }

    /// The span of `trace` whose details are open: without `span`, the one the trace opens on
    /// ([`Trace::default_span`]); with it, the span it names when the trace has it, and none
    /// for [`TraceParams::NO_SPAN`] or anything else that is not one of its spans.
    pub fn selected_span(&self, trace: &Trace) -> Option<SpanId> {
        match self.get("span") {
            None => trace.default_span().map(|span| span.span().span_id),
            Some(_) => self.span().filter(|id| trace.span(*id).is_some()),
        }
    }

    /// The tab with the span details closed: `span` set to [`TraceParams::NO_SPAN`], so the
    /// trace does not open on its default span again, and everything else kept, the page
    /// included.
    pub fn close_span(&self) -> String {
        self.href(&[("page", self.get("page")), ("span", Some(Self::NO_SPAN))])
    }

    /// The options a form keeps as hidden fields: all but those it edits (`edited`) and the
    /// page.
    pub fn keep(&self, edited: &[&str]) -> Vec<(&'static str, String)> {
        self.given
            .iter()
            .filter(|(key, _)| *key != "page" && !edited.contains(key))
            .cloned()
            .collect()
    }

    /// The tab as the Logs tab links it, with the stack and range picked there where this tab
    /// takes them: not hoster's own records' `stack`, which names no trace, and a `since` only
    /// when it is one of [`TraceParams::RANGES`]. Its link and its count are this tab's on
    /// arrival.
    pub fn linked(stack: Option<&str>, since: Option<&str>) -> Self {
        let stack = stack.filter(|stack| *stack != LogScope::HOSTER);
        let since = since.filter(|since| Self::RANGES.iter().any(|(r, _)| r == since));
        [("stack", stack), ("since", since)]
            .into_iter()
            .filter_map(|(key, value)| Some((key.to_owned(), value?.to_owned())))
            .collect::<Vec<_>>()
            .into()
    }

    /// The stack picked, when given.
    pub fn stack(&self) -> Option<&str> {
        self.get("stack")
    }

    /// The range preset in effect and how long it is; the default when `since` is not one,
    /// with why.
    pub fn range(&self) -> ((&'static str, Duration), Option<String>) {
        let default = Self::RANGES
            .into_iter()
            .find(|(label, _)| *label == Self::DEFAULT_RANGE)
            .expect("the default is a preset");
        let Some(since) = self.get("since") else {
            return (default, None);
        };
        match Self::RANGES.into_iter().find(|(label, _)| *label == since) {
            Some(range) => (range, None),
            None => (
                default,
                Some(format!(
                    "`{since}` is not a range of the trace list (5m, 15m, 1h, 6h or 24h); \
                     showing the last {}.",
                    Self::DEFAULT_RANGE
                )),
            ),
        }
    }

    /// The store's scope: one stack or all. hoster's own records' `_hoster` names no trace.
    fn scope(&self) -> store::LogScope {
        LogScope::All {
            stack: self.stack().map(str::to_owned),
        }
        .store()
    }

    /// The store's query for these options at `now`, with why any option was not taken.
    pub fn query(&self, now: SystemTime) -> (TraceQuery, Vec<String>) {
        let mut problems = Vec::new();
        let ((_, ago), problem) = self.range();
        problems.extend(problem);
        let end = Timestamp::from(now) + TracesView::END_SLACK;
        let start = Timestamp::from(now.checked_sub(ago).unwrap_or(SystemTime::UNIX_EPOCH));
        let mut query = TraceQuery::new(start..end, self.scope());
        query.name = self.get("q").map(str::to_owned);
        if let Some(status) = self.get("status") {
            match status.parse() {
                Ok(status) => query.status = Some(status),
                Err(_) => problems.push(format!(
                    "`{status}` is not a status (ok or error); showing every trace."
                )),
            }
        }
        if let Some(slower) = self.get("slower") {
            match humantime::parse_duration(slower) {
                Ok(min) => query.min_duration = Some(min),
                Err(_) => problems.push(format!(
                    "`{slower}` is not a duration such as 500ms; showing traces of any duration."
                )),
            }
        }
        if let Some(sort) = self.get("sort") {
            match sort.parse() {
                Ok(sort) => query.sort = sort,
                Err(_) => problems.push(format!(
                    "`{sort}` is not an order (newest, slowest or errors); showing the newest."
                )),
            }
        }
        if let Some(page) = self.get("page") {
            match page.parse() {
                Ok(page) => query.page = page,
                Err(_) => problems.push(format!("`{page}` is not a page; showing the first.")),
            }
        }
        (query, problems)
    }

    /// The selected trace's id, when given, or why it is not one.
    fn trace(&self) -> Option<Result<TraceId, String>> {
        self.get("trace").map(|id| {
            id.to_ascii_lowercase()
                .parse()
                .map_err(|_| format!("`{id}` is not a trace id: 32 hex digits."))
        })
    }

    /// Whether status, a minimum duration or grep narrow the list.
    fn filtered(&self) -> bool {
        ["q", "status", "slower"]
            .iter()
            .any(|key| self.get(key).is_some())
    }
}

/// The colour classes of services, assigned in the order they are first seen and the same for
/// every row and the selected trace: the categorical palette of `docs/design/README.md`, with
/// the proxy's spans ([`Span::EDGE_SERVICE`]) in their own colour (ADR-0054: classes, never
/// inline styles).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServicePalette {
    /// Every service but the proxy's, as first seen.
    services: Vec<String>,
}

impl ServicePalette {
    /// The palette of `services`, in this order.
    fn learn<'a>(&mut self, services: impl IntoIterator<Item = &'a str>) {
        for service in services {
            if service != Span::EDGE_SERVICE && !self.services.iter().any(|s| s == service) {
                self.services.push(service.to_owned());
            }
        }
    }

    /// The class of `service`: `svc-hoster` for the proxy's, `svc-0`… in turn otherwise.
    pub fn class(&self, service: &str) -> &'static str {
        if service == Span::EDGE_SERVICE {
            return LogsView::HOSTER_CLASS;
        }
        let n = self.services.iter().position(|s| s == service).unwrap_or(0);
        LogsView::PALETTE[n % LogsView::PALETTE.len()]
    }
}

/// A service of a row, with its colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceChip {
    pub name: String,
    pub class: &'static str,
}

/// One row of the trace list.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceRow {
    /// The root's start as shown: the time of day in UTC, with the date when the page spans
    /// more than one day.
    pub shown: String,
    /// The root's start in RFC 3339, UTC, to the millisecond.
    pub time: String,
    pub name: String,
    pub stack: String,
    /// Where selecting it goes.
    pub href: String,
    /// It is the selected trace.
    pub current: bool,
    pub services: Vec<ServiceChip>,
    /// The app was called but exported nothing: "app not instrumented".
    pub uninstrumented: bool,
    pub spans: String,
    /// The root's duration, as text and as a bar scaled to the slowest listed.
    pub duration: String,
    pub bar: Meter,
    pub status: TraceStatus,
    /// The status's class: [`TracesView::status_class`].
    pub status_class: &'static str,
}

/// What is selected below the list.
#[derive(Debug)]
pub enum Selection {
    /// No `trace` given.
    Nothing,
    /// `trace` is not a trace id: why.
    Invalid(String),
    /// No trace has this id: it expired, or never existed.
    Missing(TraceId),
    /// The trace, laid out.
    Found(Box<Waterfall>),
}

impl Selection {
    /// `spans are kept 3 days`.
    pub fn kept(&self) -> String {
        format!(
            "spans are kept {} days",
            Retention::SPAN_MAX_AGE.as_secs() / 86_400
        )
    }
}

/// An order of the sort select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortOption {
    pub value: &'static str,
    pub label: &'static str,
    pub current: bool,
}

/// A link of a range preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeLink {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

/// The Traces tab below the page's header: what `telemetry_traces.html` renders.
#[derive(Debug)]
pub struct TracesView {
    /// The options, which every link keeps.
    pub params: TraceParams,
    /// Why options were not taken; shown as the page's flash.
    pub problems: Vec<String>,
    /// The store cannot be read: why. Nothing else is read then.
    pub unreadable: Option<String>,
    pub ranges: Vec<RangeLink>,
    pub stacks: Vec<StackOption>,
    pub statuses: Vec<Chip>,
    pub sorts: Vec<SortOption>,
    pub sort: TraceSort,
    /// The grep and minimum duration as typed.
    pub q: String,
    pub slower: String,
    pub rows: Vec<TraceRow>,
    /// Traces the filters match, over every page; traces in the range and stack.
    pub matching: u64,
    pub total: Option<u64>,
    /// The Logs tab's count for this range and stack.
    pub logs: Option<Total>,
    /// The first row's place in the matching traces, from 1.
    pub first: u64,
    pub newer: Option<String>,
    pub older: Option<String>,
    /// The list without status, duration and grep, when they narrow it.
    pub clear: Option<String>,
    pub palette: ServicePalette,
    pub selection: Selection,
}

impl TracesView {
    /// Past `now`, so a span stamped by a clock slightly ahead is still listed.
    const END_SLACK: Duration = Duration::from_secs(1);
    /// The shortest bar, as a share of the slowest listed, so every row has one.
    const MIN_BAR: f64 = 0.02;

    /// The tab for `params` at `now`, reading the store behind `reader`, with the stacks of
    /// the current state (`known`) in the selector.
    pub async fn load(
        reader: &LogReader,
        params: TraceParams,
        known: &[String],
        now: SystemTime,
    ) -> Self {
        let (query, problems) = params.query(now);
        // A `trace` that is not an id is said where the trace would be, not in the flash.
        let selected = params.trace().transpose();
        let mut view = Self::controls(params, problems, known, query.sort);
        let store = match reader.store() {
            Ok(store) => store,
            Err(message) => {
                view.unreadable = Some(message);
                return view;
            }
        };
        let (list, logs, trace) = tokio::join!(
            store.traces(&query),
            store.count_records(query.range.clone(), &query.scope, LogsView::COUNT_CAP + 1),
            Self::read_trace(store, selected.as_ref().ok().copied().flatten()),
        );
        let list = match list {
            Ok(list) => list,
            Err(err) => {
                tracing::warn!(error = %err, "reading the trace list failed");
                view.unreadable = Some("The trace list cannot be read right now.".to_owned());
                return view;
            }
        };
        view.logs = logs
            .inspect_err(|err| tracing::warn!(error = %err, "counting the log records failed"))
            .ok()
            .map(Total::from);
        let mut found = None;
        view.selection = match (selected, trace) {
            (Err(message), _) => Selection::Invalid(message),
            (Ok(None), _) => Selection::Nothing,
            (Ok(Some(id)), Ok(None)) => Selection::Missing(id),
            (Ok(Some(_)), Ok(Some(trace))) => {
                found = Some(trace);
                Selection::Nothing
            }
            (Ok(Some(_)), Err(err)) => {
                tracing::warn!(error = %err, "reading a trace failed");
                Selection::Invalid("The trace cannot be read right now.".to_owned())
            }
        };
        let selected = match &view.selection {
            Selection::Missing(id) => Some(*id),
            _ => found.as_ref().map(Trace::id),
        };
        view.list(&query, list, selected);
        if let Some(trace) = found {
            // After the list's, so the list's services keep their colours.
            view.palette
                .learn(trace.spans().iter().map(|s| s.span().service.as_str()));
            let mut waterfall = Waterfall::new(trace, &view.palette, &view.params);
            waterfall.read_logs(store).await;
            view.selection = Selection::Found(Box::new(waterfall));
        }
        view
    }

    /// The trace `id`, when one is selected.
    async fn read_trace(
        store: &LogStore,
        id: Option<TraceId>,
    ) -> Result<Option<Trace>, StoreError> {
        match id {
            Some(id) => store.load_trace(id).await,
            None => Ok(None),
        }
    }

    /// The toolbar and sort select for `params`, with nothing read yet.
    fn controls(
        params: TraceParams,
        problems: Vec<String>,
        known: &[String],
        sort: TraceSort,
    ) -> Self {
        let ((since, _), _) = params.range();
        let ranges = TraceParams::RANGES
            .into_iter()
            .map(|(label, _)| RangeLink {
                label,
                href: params.href(&[("since", Some(label))]),
                current: label == since,
            })
            .collect();
        let status = params.get("status").and_then(|s| s.parse().ok());
        let mut statuses = vec![Chip {
            label: "all".to_owned(),
            href: params.href(&[("status", None)]),
            current: status.is_none(),
            class: "svc-all",
        }];
        statuses.extend(TraceStatus::ALL.into_iter().map(|s: TraceStatus| Chip {
            label: s.to_string(),
            href: params.href(&[("status", Some(s.as_str()))]),
            current: status == Some(s),
            class: Self::status_class(s),
        }));
        let sorts = TraceSort::ALL
            .into_iter()
            .map(|s| SortOption {
                value: s.as_str(),
                label: Self::sort_label(s),
                current: s == sort,
            })
            .collect();
        let stacks = Self::stacks(known, params.stack());
        Self {
            q: params.get("q").unwrap_or_default().to_owned(),
            slower: params.get("slower").unwrap_or_default().to_owned(),
            clear: params
                .filtered()
                .then(|| params.href(&[("q", None), ("status", None), ("slower", None)])),
            params,
            problems,
            unreadable: None,
            ranges,
            stacks,
            statuses,
            sorts,
            sort,
            rows: Vec::new(),
            matching: 0,
            total: None,
            logs: None,
            first: 1,
            newer: None,
            older: None,
            palette: ServicePalette::default(),
            selection: Selection::Nothing,
        }
    }

    /// The rows of `list`, the page `query` asked for, and the links around them; the trace
    /// `selected` is marked.
    fn list(&mut self, query: &TraceQuery, list: TraceList, selected: Option<TraceId>) {
        let TraceList {
            traces,
            matching,
            total,
        } = list;
        for trace in &traces {
            self.palette
                .learn(trace.services.iter().map(String::as_str));
        }
        let slowest = traces
            .iter()
            .map(|t| t.duration)
            .max()
            .unwrap_or_default()
            .as_secs_f64();
        let times: Vec<String> = traces
            .iter()
            .map(|t| humantime::format_rfc3339_millis(Timestamp::from(t.start).into()).to_string())
            .collect();
        let dated = Self::dated(&times);
        self.rows = traces
            .into_iter()
            .zip(times)
            .map(|(trace, time)| self.row(trace, time, dated, slowest, selected))
            .collect();

        let limit = u64::from(query.limit.clamp(1, store::LogQuery::MAX_LIMIT));
        let page = u64::from(query.page);
        self.first = page * limit + 1;
        self.newer = match page {
            0 => None,
            1 => Some(self.params.href(&[])),
            _ => {
                let previous = (page - 1).to_string();
                Some(self.params.href(&[("page", Some(&previous))]))
            }
        };
        self.older = ((page + 1) * limit < matching).then(|| {
            let next = (page + 1).to_string();
            self.params.href(&[("page", Some(&next))])
        });
        self.matching = matching;
        self.total = Some(total);
    }

    fn row(
        &self,
        trace: TraceSummary,
        time: String,
        dated: bool,
        slowest: f64,
        selected: Option<TraceId>,
    ) -> TraceRow {
        let duration = Elapsed(trace.duration).to_string();
        let tone = match trace.status {
            TraceStatus::Error => Tone::Fail,
            TraceStatus::Ok => Tone::Neutral,
        };
        let value = trace.duration.as_secs_f64().max(slowest * Self::MIN_BAR);
        TraceRow {
            shown: if dated {
                time[5..23].replacen('T', " ", 1)
            } else {
                time[11..23].to_owned()
            },
            time,
            href: self.params.select(trace.trace_id),
            current: selected == Some(trace.trace_id),
            services: trace
                .services
                .iter()
                .map(|name| ServiceChip {
                    name: name.clone(),
                    class: self.palette.class(name),
                })
                .collect(),
            uninstrumented: trace.proxy_only && trace.spans > 1,
            spans: Count(trace.spans).to_string(),
            bar: Meter::new(
                value,
                slowest,
                tone,
                format_args!("{duration}, against the slowest listed"),
            ),
            duration,
            name: trace.name,
            stack: trace.stack,
            status: trace.status,
            status_class: Self::status_class(trace.status),
        }
    }

    /// Whether rows at `times` (RFC 3339, UTC) need their date: when any falls on another day
    /// than the first, in whatever order the page sorts them.
    fn dated(times: &[String]) -> bool {
        let day = |time: &String| time.get(..10).map(str::to_owned);
        let first = times.first().map(day);
        times.iter().any(|time| Some(day(time)) != first)
    }

    /// The stack selector: every stack, then the current state's by name and the one picked
    /// even when the state has no such stack.
    fn stacks(known: &[String], picked: Option<&str>) -> Vec<StackOption> {
        let mut names: Vec<&str> = known.iter().map(String::as_str).collect();
        names.extend(picked);
        names.sort_unstable();
        names.dedup();
        let option = |value: &str| StackOption {
            value: value.to_owned(),
            label: if value.is_empty() {
                "all stacks".to_owned()
            } else {
                value.to_owned()
            },
            current: picked.unwrap_or_default() == value,
        };
        std::iter::once("").chain(names).map(option).collect()
    }

    /// The class of a status's dot and text: `trace-ok` or `trace-error`.
    pub fn status_class(status: TraceStatus) -> &'static str {
        match status {
            TraceStatus::Ok => "trace-ok",
            TraceStatus::Error => "trace-error",
        }
    }

    /// The sort select's label of `sort`.
    fn sort_label(sort: TraceSort) -> &'static str {
        match sort {
            TraceSort::Newest => "newest",
            TraceSort::Slowest => "slowest",
            TraceSort::ErrorsFirst => "errors first",
        }
    }

    /// `newest first`, as the list's heading says it.
    pub fn order(&self) -> &'static str {
        match self.sort {
            TraceSort::Newest => "newest first",
            TraceSort::Slowest => "slowest first",
            TraceSort::ErrorsFirst => "errors first",
        }
    }

    /// The heading's count: `9 of 18,204`, `51–100 of 312 matching · 18,204 in range`.
    pub fn counted(&self) -> String {
        let shown = u64::try_from(self.rows.len()).unwrap_or(u64::MAX);
        let rows = if self.first > 1 && shown > 0 {
            format!("{}–{}", Count(self.first), Count(self.first + shown - 1))
        } else {
            Count(shown).to_string()
        };
        let total = self.total.unwrap_or_default();
        if self.matching == total {
            format!("{rows} of {}", Count(total))
        } else {
            format!(
                "{rows} of {} matching · {} in range",
                Count(self.matching),
                Count(total)
            )
        }
    }

    /// The hidden fields of the toolbar's form.
    pub fn form_keep(&self) -> Vec<(&'static str, String)> {
        self.params.keep(&TraceParams::FORM)
    }

    /// The hidden fields of the sort select's form.
    pub fn sort_keep(&self) -> Vec<(&'static str, String)> {
        self.params.keep(&["sort"])
    }

    /// The first page of the list as it is.
    pub fn first_page(&self) -> String {
        self.params.href(&[])
    }
}

#[cfg(test)]
mod tests;
