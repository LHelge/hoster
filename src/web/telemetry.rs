//! The Telemetry page (ADR-0067, ADR-0068, epic psq): logs across every stack, under a header
//! of the log store's figures and a tab bar of signals. Its Logs tab (`/telemetry/logs`)
//! mounts the stored logs view of `web/logs.rs` across stacks ([`LogScope::All`]): every
//! filter the stack logs tab has, with the same parameters, a stack column and a stack
//! selector (`stack`: every stack, one, or hoster's own records that name none). It does not
//! follow, and has no source switch: those are the stack tab's. `/telemetry` redirects to the
//! Logs tab until the Traces tab (epic qnh) exists.
//!
//! The header says what hoster has of the mockup's: records per second over the last minute,
//! the 7 days the store keeps them and the size of `logs.db`. The OTLP receiver's line comes
//! with that epic.

use std::time::Duration;

use askama::Template;
use axum::Extension;
use axum::extract::Query;
use axum::response::{IntoResponse, Redirect, Response};

use super::logs::{LogMount, LogParams, LogReader, LogScope, LogsView};
use super::pages::{Flash, FlashKind, Layout, Page, Section, Shell};
use super::stacks::Stacks;
use crate::machine::Memory;
use crate::store::{self, Retention, Timestamp};

/// The Telemetry page's handlers.
pub struct Telemetry;

impl Telemetry {
    /// The page, which redirects to its first tab.
    pub const PATH: &str = "/telemetry";
    /// The Logs tab.
    pub const LOGS: &str = "/telemetry/logs";

    /// `GET /telemetry`: the Logs tab, the only one so far.
    pub async fn index() -> Redirect {
        Redirect::to(Self::LOGS)
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
        let (states, figures) = tokio::join!(stacks.status(), LogFigures::read(&reader));
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
        Page(TelemetryLogsPage {
            layout,
            figures,
            view,
        })
        .into_response()
    }
}

/// The header's figures of the log store; `None` where one could not be read.
#[derive(Debug, Default)]
struct LogFigures {
    /// Records per second over the last minute.
    rate: Option<PerSecond>,
    /// The bytes `logs.db`'s rows take, as its size cap measures them.
    size: Option<u64>,
}

impl LogFigures {
    /// The window the rate is taken over.
    const RATE_WINDOW: Duration = Duration::from_secs(60);

    async fn read(reader: &LogReader) -> Self {
        let Ok(store) = reader.store() else {
            return Self::default();
        };
        let now = Timestamp::now();
        let window = (now - Self::RATE_WINDOW)..(now + Duration::from_secs(1));
        let (count, size) = tokio::join!(
            store.count_records(window, &store::LogScope::All, LogsView::COUNT_CAP),
            store.used_bytes(),
        );
        let warn = |err: &store::StoreError| {
            tracing::warn!(error = %err, "reading the log store's figures failed");
        };
        Self {
            rate: count.inspect_err(warn).ok().map(|count| PerSecond {
                count,
                window: Self::RATE_WINDOW,
            }),
            size: size.inspect_err(warn).ok(),
        }
    }

    /// `retention 7d · 412 MiB`.
    fn retention(&self) -> String {
        let days = Retention::MAX_AGE.as_secs() / 86_400;
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

/// `GET /telemetry/logs`.
#[derive(Template)]
#[template(path = "telemetry_logs.html")]
struct TelemetryLogsPage {
    layout: Layout,
    figures: LogFigures,
    view: LogsView,
}

#[cfg(test)]
mod tests;
