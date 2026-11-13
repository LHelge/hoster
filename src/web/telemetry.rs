//! The Telemetry page (ADR-0067, ADR-0068, ADR-0069, epics psq and qnh): traces and logs across
//! every stack, under a header of the receiver's and the log store's figures and a tab bar of
//! signals. `/telemetry` redirects to its first tab, Traces.
//!
//! The Traces tab (`/telemetry/traces`, `traces.rs`) lists traces by their root over a range,
//! and shows the one `?trace=` picks below the list. The Logs tab (`/telemetry/logs`) mounts the
//! stored logs view of `web/logs.rs` across stacks ([`LogScope::All`]): every filter the stack
//! logs tab has, with the same parameters, a stack column and a stack selector (`stack`: every
//! stack, one, or hoster's own records that name none). It does not follow, and has no source
//! switch: those are the stack tab's. The tabs share `stack` and `since`: each tab's link keeps
//! them, and each tab counts what the range holds.
//!
//! The header says what hoster has of the mockup's: the OTLP receiver's state (it serves
//! OTLP/HTTP on the proxy's `:80`, ADR-0069; no gRPC port, OQ-42 question 4), spans and records
//! per second over the last minute, how long the tab's signal is kept and the size of
//! `logs.db`.

use askama::Template;
use axum::Extension;
use axum::extract::Query;
use axum::response::{IntoResponse, Redirect, Response};

use std::ops::Range;
use std::time::{Duration, SystemTime};

use self::traces::{TraceParams, TracesView};
use super::logs::{LogMount, LogParams, LogReader, LogScope, LogsView, SourcePlan, Total};
use super::pages::{Flash, FlashKind, Layout, Page, Section, Shell};
use super::stacks::Stacks;
use super::ui::Count;
use crate::machine::Memory;
use crate::store::{Retention, SpanId, Timestamp, TraceId};

mod traces;

/// The Telemetry page's handlers.
pub struct Telemetry;

impl Telemetry {
    /// The page, which redirects to its first tab.
    pub const PATH: &str = "/telemetry";
    /// The Traces tab, the first.
    pub const TRACES: &str = TracesView::PATH;
    /// The Logs tab.
    pub const LOGS: &str = "/telemetry/logs";

    /// The Traces tab with `trace` picked and `span` selected in its waterfall; without a
    /// span, the trace opens on its focus. Where a record or a request row links to its trace.
    pub fn trace_href(trace: TraceId, span: Option<SpanId>) -> String {
        let trace = trace.to_string();
        let span = span.map(|id| id.to_string());
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("trace", &trace)
            .extend_pairs(span.iter().map(|span| ("span", span)))
            .finish();
        format!("{}?{query}", Self::TRACES)
    }

    /// The Logs tab with only the records made in `trace`, at every level, across stacks and
    /// over all the store keeps (a trace filter's range, [`LogParams`]): "logs in trace".
    pub fn trace_logs_href(trace: TraceId) -> String {
        format!("{}?trace={trace}", Self::LOGS)
    }

    /// `GET /telemetry`: the Traces tab.
    pub async fn index() -> Redirect {
        Redirect::to(Self::TRACES)
    }

    /// `GET /telemetry/traces[?stack=…&since=…&q=…&sort=…&status=…&slower=…&page=…&trace=…&span=…]`:
    /// the trace list across stacks, and the trace picked.
    pub async fn traces(
        Extension(stacks): Extension<Stacks>,
        Extension(reader): Extension<LogReader>,
        shell: Shell,
        Query(params): Query<TraceParams>,
    ) -> Response {
        let mut layout = shell.layout(Section::Telemetry);
        let now = SystemTime::now();
        let (range, _) = params.range(now);
        let scope = params.scope();
        let (states, figures, logs) = tokio::join!(
            stacks.status(),
            Figures::read(&reader, Signal::Traces),
            Tabs::count_logs(&reader, range, &scope),
        );
        let known: Vec<String> = states
            .inspect_err(|err| tracing::warn!(error = %err, "listing the stacks failed"))
            .unwrap_or_default()
            .into_iter()
            .map(|status| status.name)
            .collect();
        let view = TracesView::load(reader.store(), &params, &known, now).await;
        if let Some(message) = view.problem.clone() {
            layout = layout.with_flash(Flash {
                kind: FlashKind::Error,
                message,
            });
        }
        let tabs = Tabs::new(
            Signal::Traces,
            params.stack(),
            params.since_given(),
            view.total,
            logs,
        );
        Page(TelemetryTracesPage {
            layout,
            figures,
            tabs,
            view,
        })
        .into_response()
    }

    /// `GET /telemetry/logs[?stack=…&service=…&since=…&level=…&q=…&field=…&before=…|after=…]`:
    /// the Logs tab across stacks.
    pub async fn logs(
        Extension(stacks): Extension<Stacks>,
        Extension(reader): Extension<LogReader>,
        shell: Shell,
        Query(params): Query<LogParams>,
    ) -> Response {
        let mut layout = shell.layout(Section::Telemetry);
        let scope = LogScope::All {
            stack: params.stack().map(str::to_owned),
        };
        let range = SourcePlan::new(&params.clone().scoped(&scope), SystemTime::now()).range;
        let (states, figures, traces) = tokio::join!(
            stacks.status(),
            Figures::read(&reader, Signal::Logs),
            Tabs::count_traces(&reader, range, &scope),
        );
        let states = states
            .inspect_err(|err| tracing::warn!(error = %err, "listing the stacks failed"))
            .unwrap_or_default();
        // A stack picked that is in the current state gets its revision's service chips.
        let services = states
            .iter()
            .find(|status| Some(status.name.as_str()) == params.stack())
            .map(|status| status.services.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default();
        let mount = LogMount {
            scope,
            action: Self::LOGS.to_owned(),
            services,
            live: Vec::new(),
            stream: None,
            download: None,
            stacks: states.into_iter().map(|status| status.name).collect(),
        };
        let view = LogsView::load(&reader, mount, &params).await;
        if let Some(message) = view.problem.clone() {
            layout = layout.with_flash(Flash {
                kind: FlashKind::Error,
                message,
            });
        }
        let tabs = Tabs::new(
            Signal::Logs,
            params.stack(),
            params.since_given(),
            traces,
            view.total,
        );
        Page(TelemetryLogsPage {
            layout,
            figures,
            tabs,
            view,
        })
        .into_response()
    }
}

/// The page's signals, one tab each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Traces,
    Logs,
}

impl Signal {
    /// How long the store keeps the signal.
    fn kept(self) -> Duration {
        match self {
            Signal::Traces => Retention::SPAN_MAX_AGE,
            Signal::Logs => Retention::MAX_AGE,
        }
    }
}

/// The tab bar: each tab's link, which keeps the stack and range, and what it counts in them.
#[derive(Debug)]
struct Tabs {
    current: Signal,
    traces_href: String,
    logs_href: String,
    /// The traces the range holds; `None` when unread.
    traces: Option<Count>,
    /// The records the range holds, counted up to a cap; `None` when unread.
    logs: Option<Total>,
}

impl Tabs {
    fn new(
        current: Signal,
        stack: Option<&str>,
        since: Option<&str>,
        traces: Option<u64>,
        logs: Option<Total>,
    ) -> Self {
        let pairs: Vec<(&str, &str)> = [("stack", stack), ("since", since)]
            .into_iter()
            .filter_map(|(key, value)| Some((key, value?)))
            .collect();
        let href = |path: &str| match pairs.is_empty() {
            true => path.to_owned(),
            false => format!(
                "{path}?{}",
                form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(&pairs)
                    .finish()
            ),
        };
        Self {
            current,
            traces_href: href(Telemetry::TRACES),
            logs_href: href(Telemetry::LOGS),
            traces: traces.map(Count),
            logs,
        }
    }

    fn on_traces(&self) -> bool {
        self.current == Signal::Traces
    }

    /// The records `range` and `scope` hold, for the Logs tab's count on the Traces tab.
    async fn count_logs(
        reader: &LogReader,
        range: Range<Timestamp>,
        scope: &LogScope,
    ) -> Option<Total> {
        let store = reader.store().ok()?;
        store
            .count_records(range, &scope.store(), LogsView::COUNT_CAP + 1)
            .await
            .inspect_err(|err| tracing::warn!(error = %err, "counting the log records failed"))
            .ok()
            .map(Total::from)
    }

    /// The traces `range` and `scope` hold, for the Traces tab's count on the Logs tab.
    async fn count_traces(
        reader: &LogReader,
        range: Range<Timestamp>,
        scope: &LogScope,
    ) -> Option<u64> {
        let store = reader.store().ok()?;
        store
            .count_traces(range, &scope.store())
            .await
            .inspect_err(|err| tracing::warn!(error = %err, "counting the traces failed"))
            .ok()
    }
}

/// The OTLP receiver's state as the header shows it. It serves whenever the admin app does
/// (both start in the same startup step); what it receives is stored only with a log store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiverState {
    /// Receiving and storing.
    Up,
    /// No log store: what it receives is dropped.
    NoStore,
}

impl ReceiverState {
    /// The class of its dot: its tone.
    fn class(self) -> &'static str {
        match self {
            ReceiverState::Up => "dot-ok",
            ReceiverState::NoStore => "dot-fail",
        }
    }

    /// What follows `otlp receiver`.
    fn says(self) -> &'static str {
        match self {
            ReceiverState::Up => "http :80",
            ReceiverState::NoStore => "not storing: the log store is unavailable",
        }
    }
}

/// The header's figures: the receiver's state, then the log store's; `None` where one could
/// not be read.
#[derive(Debug)]
struct Figures {
    receiver: ReceiverState,
    /// Spans stored per second over the last minute.
    spans: Option<PerSecond>,
    /// Records per second over the last minute.
    logs: Option<PerSecond>,
    /// The bytes `logs.db`'s rows take, as its size cap measures them.
    size: Option<u64>,
    /// The tab's signal, whose retention the header names.
    signal: Signal,
}

impl Figures {
    /// The window the rates are taken over.
    const RATE_WINDOW: Duration = Duration::from_secs(60);

    async fn read(reader: &LogReader, signal: Signal) -> Self {
        let Ok(store) = reader.store() else {
            return Self {
                receiver: ReceiverState::NoStore,
                spans: None,
                logs: None,
                size: None,
                signal,
            };
        };
        let since = Timestamp::now() - Self::RATE_WINDOW;
        let (rows, size) = tokio::join!(store.rows_since(since), store.used_bytes());
        let warn = |err: &crate::store::StoreError| {
            tracing::warn!(error = %err, "reading the log store's figures failed");
        };
        let rows = rows.inspect_err(warn).ok();
        let rate = |count| PerSecond {
            count,
            window: Self::RATE_WINDOW,
        };
        Self {
            receiver: ReceiverState::Up,
            spans: rows.map(|r| rate(r.spans)),
            logs: rows.map(|r| rate(r.records)),
            size: size.inspect_err(warn).ok(),
            signal,
        }
    }

    /// `retention 7d · 412 MiB`, for the tab's signal.
    fn retention(&self) -> String {
        let days = self.signal.kept().as_secs() / 86_400;
        match self.size {
            Some(size) => format!("retention {days}d · {}", Memory(size)),
            None => format!("retention {days}d"),
        }
    }
}

/// A count over a window as a rate per second: one decimal below ten, whole from there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PerSecond {
    count: u64,
    window: Duration,
}

impl std::fmt::Display for PerSecond {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[expect(clippy::cast_precision_loss, reason = "a rate shown to one decimal")]
        let rate = self.count as f64 / self.window.as_secs_f64().max(1.0);
        if rate < 10.0 {
            write!(f, "{rate:.1}")
        } else {
            write!(f, "{}", super::ui::Count(rate.round() as u64))
        }
    }
}

/// `GET /telemetry/traces`.
#[derive(Template)]
#[template(path = "telemetry_traces.html")]
struct TelemetryTracesPage {
    layout: Layout,
    figures: Figures,
    tabs: Tabs,
    view: TracesView,
}

/// `GET /telemetry/logs`.
#[derive(Template)]
#[template(path = "telemetry_logs.html")]
struct TelemetryLogsPage {
    layout: Layout,
    figures: Figures,
    tabs: Tabs,
    view: LogsView,
}

#[cfg(test)]
mod tests;
