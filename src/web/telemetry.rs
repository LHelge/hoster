//! The Telemetry page (ADR-0067, ADR-0068, ADR-0069; epics psq and qnh): traces and logs
//! across every stack, under one header of the receiver's and the log store's figures and a
//! tab bar of signals ([`Signal`]), each tab its own URL and each with its count for the range
//! and stack in view. `/telemetry` redirects to the first, the Traces tab.
//!
//! - **Traces** (`/telemetry/traces`, `telemetry/traces.rs`): the trace list over a range with
//!   its sort and filters, and the trace selected by `?trace=` below it.
//! - **Logs** (`/telemetry/logs`) mounts the stored logs view of `web/logs.rs` across stacks
//!   ([`LogScope::All`]): every filter the stack logs tab has, with the same parameters, a
//!   stack column and a stack selector (`stack`: every stack, one, or hoster's own records that
//!   name none). It does not follow, and has no source switch: those are the stack tab's.
//!
//! The tabs link each other with the stack picked and the range, where the other tab offers
//! it. The header's receiver line says whether what the OTLP receiver gets is stored (the log
//! store is open), where services send it (OTLP/HTTP only, OQ-42), and the spans and log
//! records stored per second over the last minute; then the retention and the size of
//! `logs.db`.

mod traces;

use std::time::{Duration, SystemTime};

use askama::Template;
use axum::Extension;
use axum::extract::Query;
use axum::response::{IntoResponse, Redirect, Response};

use self::traces::{Selection, TraceParams, TracesView};
use super::logs::{LogMount, LogParams, LogReader, LogScope, LogsView};
use super::pages::{Flash, FlashKind, Layout, Page, Section, Shell};
use super::stacks::Stacks;
use super::ui::Count;
use crate::machine::Memory;
use crate::otlp::OtlpReceiver;
use crate::store::{Retention, Timestamp};

/// The Telemetry page's handlers.
pub struct Telemetry;

impl Telemetry {
    /// The page, which redirects to its first tab.
    pub const PATH: &str = "/telemetry";
    /// The Traces tab.
    pub const TRACES: &str = TraceParams::PATH;
    /// The Logs tab.
    pub const LOGS: &str = "/telemetry/logs";

    /// `GET /telemetry`: the Traces tab.
    pub async fn index() -> Redirect {
        Redirect::to(Self::TRACES)
    }

    /// `GET /telemetry/traces[?stack=…&since=…&q=…&status=…&slower=…&sort=…&page=…&trace=…]`:
    /// the Traces tab across stacks.
    pub async fn traces(
        Extension(stacks): Extension<Stacks>,
        Extension(reader): Extension<LogReader>,
        shell: Shell,
        Query(params): Query<TraceParams>,
    ) -> Response {
        let mut layout = shell.layout(Section::Telemetry);
        let (known, figures) = tokio::join!(Self::stack_names(&stacks), Figures::read(&reader));
        let view = TracesView::load(&reader, params, &known, SystemTime::now()).await;
        if !view.problems.is_empty() {
            layout = layout.with_flash(Flash {
                kind: FlashKind::Error,
                message: view.problems.join(" "),
            });
        }
        let stack = view.params.stack();
        let tabs = vec![
            Tab {
                signal: Signal::Traces,
                href: view.params.href(&[]),
                count: view.total.map(|total| Count(total).to_string()),
                current: true,
            },
            Tab {
                signal: Signal::Logs,
                href: Signal::Logs.href(stack, view.params.get("since")),
                count: view.logs.map(|total| total.to_string()),
                current: false,
            },
        ];
        Page(TelemetryTracesPage {
            layout,
            head: Head {
                current: Signal::Traces,
                figures,
                tabs,
            },
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
        // The Traces tab's count is of what its link opens: its own range and stack.
        let linked = TraceParams::linked(params.stack(), params.since_param());
        let (query, _) = linked.query(SystemTime::now());
        let traces = async {
            let store = reader.store().ok()?;
            store
                .count_traces(query.range, query.scope)
                .await
                .inspect_err(|err| tracing::warn!(error = %err, "counting the traces failed"))
                .ok()
        };
        let (states, figures, traces) =
            tokio::join!(stacks.status(), Figures::read(&reader), traces);
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
        let stack = params.stack();
        let tabs = vec![
            Tab {
                signal: Signal::Traces,
                href: linked.href(&[]),
                count: traces.map(|total| Count(total).to_string()),
                current: false,
            },
            Tab {
                signal: Signal::Logs,
                href: Signal::Logs.href(stack, params.since_param()),
                count: view.total.map(|total| total.to_string()),
                current: true,
            },
        ];
        Page(TelemetryLogsPage {
            layout,
            head: Head {
                current: Signal::Logs,
                figures,
                tabs,
            },
            view,
        })
        .into_response()
    }

    /// The stacks of the current state, by name; none, logged, when they cannot be listed.
    async fn stack_names(stacks: &Stacks) -> Vec<String> {
        stacks
            .status()
            .await
            .inspect_err(|err| tracing::warn!(error = %err, "listing the stacks failed"))
            .unwrap_or_default()
            .into_iter()
            .map(|status| status.name)
            .collect()
    }
}

/// A tab of the Telemetry page, in the tab bar's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Traces,
    Logs,
}

impl Signal {
    fn label(self) -> &'static str {
        match self {
            Signal::Traces => "Traces",
            Signal::Logs => "Logs",
        }
    }

    /// The crumb's path, after `~/`.
    fn path(self) -> &'static str {
        match self {
            Signal::Traces => "telemetry/traces",
            Signal::Logs => "telemetry/logs",
        }
    }

    /// This tab with the other's stack and range, where it takes them.
    fn href(self, stack: Option<&str>, since: Option<&str>) -> String {
        let path = match self {
            Signal::Traces => return TraceParams::linked(stack, since).href(&[]),
            Signal::Logs => Telemetry::LOGS,
        };
        let pairs: Vec<(&str, &str)> = [("stack", stack), ("since", since)]
            .into_iter()
            .filter_map(|(key, value)| Some((key, value?)))
            .collect();
        if pairs.is_empty() {
            return path.to_owned();
        }
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        format!("{path}?{query}")
    }
}

/// A tab of the tab bar.
#[derive(Debug)]
struct Tab {
    signal: Signal,
    href: String,
    /// Its count for the range and stack; `None` where it could not be read.
    count: Option<String>,
    current: bool,
}

/// The page's header: crumb, figures and tab bar (`telemetry_head.html`).
#[derive(Debug)]
struct Head {
    current: Signal,
    figures: Figures,
    tabs: Vec<Tab>,
}

/// The header's figures; `None` where one could not be read.
#[derive(Debug, Default)]
struct Figures {
    /// Why the log store, and so the receiver, stores nothing; `None` while it is open.
    unavailable: Option<String>,
    /// Log records per second over the last minute.
    logs: Option<PerSecond>,
    /// Spans stored per second over the last minute, by their start: the receiver's.
    spans: Option<PerSecond>,
    /// The bytes `logs.db`'s rows take, as its size cap measures them.
    size: Option<u64>,
}

impl Figures {
    /// The window the rates are taken over.
    const RATE_WINDOW: Duration = Duration::from_secs(60);

    async fn read(reader: &LogReader) -> Self {
        let store = match reader.store() {
            Ok(store) => store,
            Err(message) => {
                return Self {
                    unavailable: Some(message),
                    ..Self::default()
                };
            }
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
            unavailable: None,
            logs: rows.map(|rows| rate(rows.records)),
            spans: rows.map(|rows| rate(rows.spans)),
            size: size.inspect_err(warn).ok(),
        }
    }

    /// The receiver's dot: storing or not.
    fn receiver_class(&self) -> &'static str {
        match self.unavailable {
            None => "receiver-on",
            Some(_) => "receiver-off",
        }
    }

    /// `otlp receiver · http://host.containers.internal`, or why it stores nothing.
    fn receiver(&self) -> String {
        match &self.unavailable {
            None => format!("otlp receiver · {}", OtlpReceiver::ENDPOINT),
            Some(_) => "otlp receiver · not storing: the log store is unavailable".to_owned(),
        }
    }

    /// `1.2 spans/s · 340 logs/s`, `—` for a rate not read.
    fn rates(&self) -> String {
        let rate = |rate: Option<PerSecond>| rate.map_or_else(|| "—".to_owned(), |r| r.to_string());
        format!("{} spans/s · {} logs/s", rate(self.spans), rate(self.logs))
    }

    /// `retention 7d, spans 3d · 412 MiB`.
    fn retention(&self) -> String {
        let days = |age: Duration| age.as_secs() / 86_400;
        let kept = format!(
            "retention {}d, spans {}d",
            days(Retention::MAX_AGE),
            days(Retention::SPAN_MAX_AGE)
        );
        match self.size {
            Some(size) => format!("{kept} · {}", Memory(size)),
            None => kept,
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
            write!(f, "{}", Count(rate.round() as u64))
        }
    }
}

/// `GET /telemetry/traces`.
#[derive(Template)]
#[template(path = "telemetry_traces.html")]
struct TelemetryTracesPage {
    layout: Layout,
    head: Head,
    view: TracesView,
}

/// `GET /telemetry/logs`.
#[derive(Template)]
#[template(path = "telemetry_logs.html")]
struct TelemetryLogsPage {
    layout: Layout,
    head: Head,
    view: LogsView,
}

#[cfg(test)]
mod tests;
