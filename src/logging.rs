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
//! the [`ACCESS_TARGET`] target, at `debug` only: the request rows in the log store are the
//! access log (ADR-0067 §5), so the line is there for debugging the proxy
//! (`RUST_LOG=info,hoster::access=debug`). The store logs one line per deployment transition
//! under [`DEPLOY_TARGET`], so it can be filtered on its own.
//!
//! # What anyone on the internet can make hoster log
//!
//! A proxy on a public address is probed all day, and a line per probe hides the lines that
//! matter. So what a client can cause without reaching an app is logged at `debug`, under two
//! targets of its own, and is silent by default:
//!
//! - [`UNROUTED_TARGET`]: the access line of a request that matched no route (an unknown host
//!   or prefix, a host that is not the SNI name) and of everything on `:80` but an answered
//!   ACME challenge;
//! - [`CLIENT_TARGET`]: Pingora's `error` lines for a connection the client broke before a
//!   request existed (a failed TLS or HTTP/2 handshake, a request header that never
//!   finished), re-logged by [`ClientNoise`].
//!
//! `RUST_LOG=info,hoster::access::unrouted=debug,hoster::proxy::client=debug` brings both back.
//! Nor is any of it stored in the log store: a request row is written only for a request routed
//! to a stack.
//!
//! # hoster's own records in the log store
//!
//! Beside the stderr formatter, [`Logging`] installs a second layer (ADR-0068) that turns every
//! event at `info` and above into a record of the log store, with service `hoster` and stream
//! `hoster`, so the admin UI shows hoster's lines beside the apps'. Its filter is fixed, not
//! `RUST_LOG`: what the UI shows does not depend on how the journal is tuned, and stderr is
//! unchanged by it. It leaves out the log store's own modules (`hoster::logs`,
//! `hoster::store::logs`), so recording cannot feed itself, and [`ACCESS_TARGET`], which the
//! request rows cover.
//!
//! A record's stack and generation are the event's `stack` and `generation` fields, or those of
//! the nearest enclosing span that has them ([`LogSpan`]); a span's generation is taken only for
//! its own stack. Its trace and span ids are the `trace_id` and `span_id` fields of the event or
//! of the nearest enclosing span with a valid `trace_id` (ADR-0069 §8): code that logs inside a
//! request passes the request's trace id and the proxy's server span id under those names, as
//! the proxy's access line does. Its level, message and fields are what the log store's line
//! parser makes of the object the JSON formatter writes for the event, less its `timestamp`
//! (the record's time): the other fields, `target` and `spans`.
//!
//! The layer only offers records to the recorder's channel ([`LogSink::try_record`]): logging is
//! up before the log store, so the supervisor [`connect`](HosterRecords::connect)s the sink once
//! the recorder runs and disconnects it when the recorder has stopped, and events outside that
//! window are in the journal only. hoster counts as one source under the line-rate cap of
//! ADR-0067 §7, with the same marker record ([`HosterRecords::capped`]).
//!
//! [`LogSink::try_record`]: crate::logs::LogSink::try_record

use std::fmt;
use std::io::IsTerminal;
use std::str::FromStr;

use serde::Deserialize;
use tracing::Span;
use tracing::field::Empty;
use tracing_log::{AsLog, LogTracer};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

mod records;

pub use records::HosterRecords;

/// The target of the proxy's per-request access log lines, logged at `debug`.
pub const ACCESS_TARGET: &str = "hoster::access";
/// The target of the access log lines of requests that matched no route, logged at `debug`.
/// A child of [`ACCESS_TARGET`], so turning that off turns these off too.
pub const UNROUTED_TARGET: &str = "hoster::access::unrouted";
/// The target of Pingora's lines about connections a client broke, re-logged at `debug`.
pub const CLIENT_TARGET: &str = "hoster::proxy::client";
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
    /// The stderr formatter's filter.
    filter: EnvFilter,
    /// Where events go into the log store; `None` in tests capturing lines only.
    records: Option<HosterRecords>,
}

impl From<LogFormat> for Logging {
    /// Logging in `format`, filtered by `RUST_LOG` (default `info`), and into the log store
    /// through [`HosterRecords::global`].
    fn from(format: LogFormat) -> Self {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        Self {
            format,
            filter,
            records: Some(HosterRecords::global().clone()),
        }
    }
}

impl Logging {
    /// Installs this subscriber for the whole process, writing to stderr, and forwards records
    /// of the `log` crate (Pingora logs through it) to it through [`ClientNoise`]. A second
    /// call (or a subscriber installed elsewhere first) leaves the existing one in place.
    pub fn init(self) {
        let ansi = self.format == LogFormat::Text && std::io::stderr().is_terminal();
        let subscriber = self.subscriber(std::io::stderr, ansi);
        // No `log` record above the most verbose level the filter lets through is built.
        let max_level = subscriber.max_level_hint();
        if tracing::subscriber::set_global_default(subscriber).is_ok()
            && log::set_boxed_logger(Box::new(ClientNoise::default())).is_ok()
        {
            log::set_max_level(max_level.map_or(log::LevelFilter::Trace, |level| level.as_log()));
        }
    }

    /// The subscriber writing to `writer`, and recording into the log store if it has
    /// [`HosterRecords`]; `ansi` colours text output. Each layer has its own filter.
    fn subscriber<W>(self, writer: W, ansi: bool) -> Box<dyn tracing::Subscriber + Send + Sync>
    where
        W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
    {
        let layer = tracing_subscriber::fmt::layer().with_writer(writer);
        let formatter = match self.format {
            LogFormat::Text => layer
                .compact()
                .with_ansi(ansi)
                .with_filter(self.filter)
                .boxed(),
            LogFormat::Json => layer
                .json()
                .flatten_event(true)
                .with_current_span(false)
                .with_span_list(true)
                .with_filter(self.filter)
                .boxed(),
        };
        let records = self.records.map(|records| records.layer());
        Box::new(Registry::default().with(formatter).with(records))
    }
}

/// The process's `log` logger: every record goes on to `tracing`, except Pingora's `error`
/// lines about a connection the client broke, which are logged again at `debug` under
/// [`CLIENT_TARGET`].
///
/// Pingora logs those at `error` whoever caused them. On `:443` that is every port scan and
/// every client without a name hoster has a certificate for (there is no default certificate,
/// ADR-0032). They are recognised by where Pingora logs them and how the message starts,
/// since Pingora offers nothing better; `tests/supervisor.rs` fails when an upgrade changes
/// the wording. Pingora's other errors (a failed `accept`, an upstream that cannot be
/// reached, a panic) are not touched.
#[derive(Default)]
struct ClientNoise(LogTracer);

impl ClientNoise {
    /// Where Pingora 0.9 logs a client's failure, and how each such message starts.
    const SOURCES: [(&str, &[&str]); 3] = [
        // A TLS handshake that failed or timed out.
        (
            "pingora_core::services::listening",
            &["Downstream handshake"],
        ),
        // An `h2` connection without a valid preface.
        ("pingora_core::apps", &["H2 handshake error"]),
        // A request that ended before its header did, or whose client went away later.
        ("pingora_proxy", &["Fail to proxy: Downstream"]),
    ];

    /// The message of `record` if a client caused it.
    fn message(record: &log::Record<'_>) -> Option<String> {
        if record.level() != log::Level::Error {
            return None;
        }
        let (_, starts) = Self::SOURCES
            .iter()
            .find(|(target, _)| *target == record.target())?;
        let message = record.args().to_string();
        let message = message.trim_start();
        starts
            .iter()
            .any(|start| message.starts_with(start))
            .then(|| message.to_owned())
    }
}

impl log::Log for ClientNoise {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.0.enabled(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        match Self::message(record) {
            Some(message) => {
                tracing::debug!(target: CLIENT_TARGET, source = record.target(), "{message}");
            }
            None => self.0.log(record),
        }
    }

    fn flush(&self) {}
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
            records: None,
        };
        (buffer.clone(), logging.subscriber(buffer, false))
    }

    /// What [`global`] captures: `hoster` at `info`, and the access lines of routed requests,
    /// which are `debug` ones, but not those of unrouted requests.
    pub const GLOBAL_FILTER: &str =
        "off,hoster=info,hoster::access=debug,hoster::access::unrouted=info";

    /// The `hoster` lines at `info` and above of every thread in the test process that has no
    /// scoped subscriber, including Pingora's, plus the routed access lines at `debug`
    /// ([`GLOBAL_FILTER`]), as JSON. Installs the process-wide subscriber on first use.
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
                filter: EnvFilter::new(GLOBAL_FILTER),
                records: None,
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
    fn only_pingoras_client_errors_are_noise() {
        let message = |level, target: &str, args: std::fmt::Arguments<'_>| {
            let record = log::Record::builder()
                .level(level)
                .target(target)
                .args(args)
                .build();
            ClientNoise::message(&record)
        };
        let error = log::Level::Error;
        let listening = "pingora_core::services::listening";
        for (target, args) in [
            (
                listening,
                format_args!("Downstream handshake error from 203.0.113.9:1234: eof"),
            ),
            (listening, format_args!("Downstream handshake timeout")),
            (
                "pingora_core::apps",
                format_args!("H2 handshake error  HandshakeError"),
            ),
            (
                "pingora_proxy",
                format_args!("Fail to proxy: Downstream ConnectionClosed context: x"),
            ),
        ] {
            assert!(message(error, target, args).is_some(), "{target}");
        }
        for (level, target, args) in [
            // A real problem from the same place.
            (
                error,
                listening,
                format_args!("Accept() failed too many open files"),
            ),
            (
                error,
                "pingora_proxy",
                format_args!("Fail to proxy: Upstream ConnectRefused, status: 502"),
            ),
            (
                error,
                "pingora_proxy",
                format_args!("Fail to proxy: Internal InternalError"),
            ),
            // The same words from somewhere else, or at another level.
            (
                error,
                "hoster::proxy",
                format_args!("Downstream handshake timeout"),
            ),
            (
                log::Level::Warn,
                listening,
                format_args!("Downstream handshake timeout"),
            ),
        ] {
            assert_eq!(message(level, target, args), None, "{target}");
        }
    }

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
