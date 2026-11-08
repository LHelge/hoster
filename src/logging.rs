//! hoster's own logs (ADR-0044, ADR-0045): `tracing` events on stderr, which systemd puts in the
//! user journal (ADR-0023).
//!
//! [`Logging`] installs the process-wide subscriber in one of two [`LogFormat`]s, filtered by
//! `RUST_LOG` (default `info`):
//!
//! - **text**: compact human-readable lines, coloured only on a terminal;
//! - **json**: one JSON object per line with the event's fields at the top level and the
//!   fields of every enclosing span under `spans`, for `journalctl -o cat | jq`.
//!
//! Deploy code runs inside the spans of [`LogSpan`], so every line it logs carries `stack`,
//! `deployment_id`, `generation` and `service`. The proxy logs one access line per request under
//! the [`ACCESS_TARGET`] target, and the store one line per deployment transition under
//! [`DEPLOY_TARGET`], so either can be filtered on its own
//! (`RUST_LOG=info,hoster::access=off`).

use std::fmt;
use std::io::IsTerminal;
use std::str::FromStr;

use serde::Deserialize;
use tracing::Span;
use tracing::field::Empty;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::util::SubscriberInitExt;

/// The target of the proxy's per-request access log lines.
pub const ACCESS_TARGET: &str = "hoster::access";
/// The target of the deployment event log lines.
pub const DEPLOY_TARGET: &str = "hoster::deploy";

/// How log lines are written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Compact human-readable lines.
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LogFormat::Text => "text",
            LogFormat::Json => "json",
        })
    }
}

/// A log format name that is neither `text` nor `json`.
#[derive(Debug, thiserror::Error)]
#[error("unknown log format `{0}` (expected `text` or `json`)")]
pub struct UnknownLogFormat(String);

impl FromStr for LogFormat {
    type Err = UnknownLogFormat;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "text" => Ok(LogFormat::Text),
            "json" => Ok(LogFormat::Json),
            _ => Err(UnknownLogFormat(s.to_owned())),
        }
    }
}

/// The process-wide log subscriber.
pub struct Logging {
    format: LogFormat,
    filter: EnvFilter,
}

impl From<LogFormat> for Logging {
    /// Logging in `format`, filtered by `RUST_LOG` (default `info`).
    fn from(format: LogFormat) -> Self {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        Self { format, filter }
    }
}

impl Logging {
    /// Installs this subscriber for the whole process, writing to stderr, and forwards records
    /// of the `log` crate (Pingora logs through it) to it. A second call (or a subscriber
    /// installed elsewhere first) leaves the existing one in place.
    pub fn init(self) {
        let ansi = self.format == LogFormat::Text && std::io::stderr().is_terminal();
        let _ = self.subscriber(std::io::stderr, ansi).try_init();
    }

    /// The subscriber writing to `writer`; `ansi` colours text output.
    fn subscriber<W>(self, writer: W, ansi: bool) -> Box<dyn tracing::Subscriber + Send + Sync>
    where
        W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
    {
        let builder = tracing_subscriber::fmt()
            .with_env_filter(self.filter)
            .with_writer(writer);
        match self.format {
            LogFormat::Text => Box::new(builder.compact().with_ansi(ansi).finish()),
            LogFormat::Json => Box::new(
                builder
                    .json()
                    .flatten_event(true)
                    .with_current_span(false)
                    .with_span_list(true)
                    .finish(),
            ),
        }
    }
}

/// The spans deploy code runs in, so its log lines carry the deployment they belong to.
pub struct LogSpan;

impl LogSpan {
    /// The span of one deployment of `stack`. `generation` is recorded later
    /// (`span.record("generation", n)`) when the deployment has none yet. It is a root span:
    /// the deployment outlives the request (apply, webhook, poll) that started it.
    pub fn deployment(
        stack: &str,
        deployment_id: impl fmt::Display,
        generation: Option<u32>,
    ) -> Span {
        let span = tracing::info_span!(
            parent: None,
            "deployment",
            stack = %stack,
            deployment_id = %deployment_id,
            generation = Empty,
        );
        if let Some(generation) = generation {
            span.record("generation", generation);
        }
        span
    }

    /// The span of work on one service, inside a [`deployment`](Self::deployment) span or on
    /// its own (the reconciler).
    pub fn service(stack: &str, service: &str, generation: u32) -> Span {
        tracing::info_span!("service", stack = %stack, service = %service, generation)
    }
}

/// Test support: log lines captured as text, for asserting on what was logged.
#[cfg(test)]
pub mod capture {
    use std::io;
    use std::sync::{Arc, Mutex, OnceLock};

    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::fmt::MakeWriter;

    use super::{LogFormat, Logging};

    /// A shared in-memory log sink.
    #[derive(Debug, Clone, Default)]
    pub struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Buffer {
        /// Everything written so far, one entry per line.
        pub fn lines(&self) -> Vec<String> {
            let bytes = self.0.lock().unwrap().clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// The captured lines parsed as JSON.
        pub fn json(&self) -> Vec<serde_json::Value> {
            self.lines()
                .iter()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// A subscriber writing `format` lines matching `filter` into a fresh buffer, for
    /// `tracing::subscriber::with_default` or `set_default`. `filter` may enable at most
    /// `hoster=info` (see [`global`]).
    pub fn subscriber(
        format: LogFormat,
        filter: &str,
    ) -> (Buffer, Box<dyn tracing::Subscriber + Send + Sync>) {
        global();
        let buffer = Buffer::default();
        let logging = Logging {
            format,
            filter: EnvFilter::new(filter),
        };
        (buffer.clone(), logging.subscriber(buffer, false))
    }

    /// The `hoster` lines at `info` and above of every thread in the test process that has no
    /// scoped subscriber, including Pingora's, as JSON. Installs the process-wide subscriber on
    /// first use.
    ///
    /// It also keeps the scoped subscribers of [`subscriber`] reliable: `tracing` caches each
    /// callsite's interest, and while only one dispatcher is alive it asks just the current
    /// thread's. A test logging without a subscriber would then cache "never" for a callsite
    /// that another test, running in parallel, captures. With this always-alive subscriber
    /// enabling `hoster=info`, no `hoster` callsite is ever cached as "never" at `info`.
    pub fn global() -> &'static Buffer {
        static GLOBAL: OnceLock<Buffer> = OnceLock::new();
        GLOBAL.get_or_init(|| {
            let buffer = Buffer::default();
            let logging = Logging {
                format: LogFormat::Json,
                filter: EnvFilter::new("off,hoster=info"),
            };
            tracing::subscriber::set_global_default(logging.subscriber(buffer.clone(), false))
                .expect("no other global subscriber in tests");
            buffer
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::capture::subscriber;
    use super::*;

    #[test]
    fn formats_parse_and_print() {
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("Text".parse::<LogFormat>().unwrap(), LogFormat::Text);
        assert!("yaml".parse::<LogFormat>().is_err());
        assert_eq!(LogFormat::Json.to_string(), "json");
        assert_eq!(LogFormat::default(), LogFormat::Text);
        let parsed: LogFormat = serde_json::from_value(json!("json")).unwrap();
        assert_eq!(parsed, LogFormat::Json);
    }

    #[test]
    fn json_lines_carry_event_fields_and_every_span() {
        let (buffer, subscriber) = subscriber(LogFormat::Json, "info");
        tracing::subscriber::with_default(subscriber, || {
            let deployment = LogSpan::deployment("blog", 42, None);
            let _d = deployment.enter();
            deployment.record("generation", 7u32);
            let _s = LogSpan::service("blog", "web", 7).entered();
            tracing::info!(digest = "sha256:abc", "container started");
            tracing::debug!("filtered out");
        });
        let lines = buffer.json();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert_eq!(line["level"], "INFO");
        assert_eq!(line["message"], "container started");
        assert_eq!(line["digest"], "sha256:abc");
        assert_eq!(line["target"], "hoster::logging::tests");
        assert!(line["timestamp"].is_string());
        assert_eq!(
            line["spans"],
            json!([
                {"name": "deployment", "stack": "blog", "deployment_id": "42", "generation": 7},
                {"name": "service", "stack": "blog", "service": "web", "generation": 7},
            ])
        );
    }

    #[test]
    fn text_lines_show_span_context() {
        let (buffer, subscriber) = subscriber(LogFormat::Text, "info");
        tracing::subscriber::with_default(subscriber, || {
            let _d = LogSpan::deployment("blog", 3, Some(2)).entered();
            tracing::warn!(attempt = 2, "health check failed");
        });
        let lines = buffer.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        // Compact text names the span up front and appends its fields after the event's own.
        assert!(line.contains("WARN deployment: "), "{line}");
        assert!(
            line.ends_with("health check failed attempt=2 stack=blog deployment_id=3 generation=2"),
            "{line}"
        );
        assert!(!line.contains('\x1b'), "no colours off a terminal: {line}");
    }
}
