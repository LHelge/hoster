//! The details of the span selected in the waterfall, below its row (`docs/design/Telemetry.dc.html`):
//! a title line, the timing and ids, the status message of a span in error, its attributes,
//! resource and events, and the log records made in it.
//!
//! Everything here but the timing is the app's data, sent over OTLP: it is shown as text only,
//! escaped by the template, and long values wrap in CSS. Attributes, resource and events are
//! stored as JSON text (`otlp/map.rs`); what does not parse as the shape expected is left out
//! rather than failing the page.

use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

use super::super::Telemetry;
use super::{ServiceChip, TracesView};
use crate::otlp::ResourceAttributes;
use crate::stats::Elapsed;
use crate::store::{
    Level, LogEntry, MicroTimestamp, RecordKey, SpanId, SpanKind, SpanStatus, Trace, TraceSpan,
};
use crate::web::logs::LogScope;
use crate::web::ui::Count;

/// The details of one span, ready for the template.
#[derive(Debug)]
pub struct SpanDetails {
    pub id: SpanId,
    pub name: String,
    pub service: ServiceChip,
    pub kind: SpanKind,
    pub status: SpanStatus,
    /// The waterfall with no span selected.
    pub close: String,
    /// After the trace's start: `+51.7 ms`.
    pub start: String,
    pub duration: String,
    /// The duration's share of the trace: `94.1%`.
    pub share: String,
    /// The duration minus the union of the children's intervals.
    pub self_time: String,
    /// `None` for a root.
    pub parent: Option<SpanId>,
    /// The parent is not in the trace.
    pub parent_missing: bool,
    /// The status message of a span in error.
    pub message: Option<String>,
    /// By key.
    pub attributes: Vec<Attribute>,
    /// The resource attributes that say what sent the span ([`SpanDetails::RESOURCE_KEYS`]),
    /// in that order.
    pub resource: Vec<Pair>,
    pub events: Vec<SpanEvent>,
    /// The first log records made in the span, by time.
    pub logs: Vec<SpanLog>,
    /// How many more were made in it than are listed.
    pub more_logs: Option<Count>,
}

/// A key and its value as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    pub key: String,
    pub value: String,
}

/// A span attribute, `alert` when it tells of a failure: `error.type`, or a 5xx
/// `http.response.status_code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub key: String,
    pub value: String,
    pub alert: bool,
}

/// An event of the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanEvent {
    pub name: String,
    /// `@ 1.9 s (+1.8 s in span)`: after the trace's start, and after the span's.
    pub at: String,
    /// An `exception` event: shown in red.
    pub exception: bool,
    /// Its attributes but the stack trace, by key.
    pub attributes: Vec<Pair>,
    /// `exception.stacktrace`.
    pub stacktrace: Option<String>,
}

/// A log record made in the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanLog {
    /// RFC 3339 in UTC, to the millisecond.
    pub time: String,
    /// The time of day: `19:58:42.990`.
    pub shown: String,
    pub level: Option<Level>,
    pub message: String,
    /// The Logs tab from this record on.
    pub href: String,
}

impl SpanDetails {
    /// The most log records listed; [`SpanDetails::more_logs`] counts the rest.
    pub const MAX_LOGS: u32 = 50;
    /// The resource attributes shown: what sent the span, and the stack and deployment
    /// generation hoster injects (`otlp/env.rs`).
    pub const RESOURCE_KEYS: [&str; 5] = [
        "service.name",
        "service.version",
        "telemetry.sdk.language",
        ResourceAttributes::NAMESPACE,
        ResourceAttributes::GENERATION,
    ];

    /// The details of `span` of `trace`, its service as `service`, closed by `close`, with the
    /// first log records made in it.
    pub fn new(
        trace: &Trace,
        span: &TraceSpan,
        service: ServiceChip,
        close: String,
        records: Vec<LogEntry>,
    ) -> Self {
        let s = &span.span;
        let total = trace.duration();
        let share = if total.is_zero() {
            100.0
        } else {
            s.duration.as_secs_f64() / total.as_secs_f64() * 100.0
        };
        let logs: Vec<SpanLog> = records.iter().map(SpanLog::from).collect();
        let more = span.log_records.saturating_sub(logs.len() as u64);
        Self {
            id: s.span_id,
            name: s.name.clone(),
            service,
            kind: s.kind,
            status: s.status,
            close,
            start: format!("+{}", Elapsed(trace.offset(s.start))),
            duration: Elapsed(s.duration).to_string(),
            share: format!("{share:.1}%"),
            self_time: Elapsed(span.self_time).to_string(),
            parent: s.parent,
            parent_missing: span.parent_missing,
            message: s.status_message.clone().filter(|_| s.is_error()),
            attributes: Self::object(s.attributes.as_deref())
                .into_iter()
                .map(|(key, value)| Attribute {
                    alert: Attribute::alerts(&key, &value),
                    value: Text(&value).to_string(),
                    key,
                })
                .collect(),
            resource: Self::resource(s.resource.as_deref()),
            events: Self::events(trace, s.start, s.events.as_deref()),
            logs,
            more_logs: (more > 0).then_some(Count(more)),
        }
    }

    /// The class of the status pill.
    pub fn status_class(&self) -> &'static str {
        match self.status {
            SpanStatus::Error => "sd-status-error",
            SpanStatus::Ok => "sd-status-ok",
            SpanStatus::Unset => "sd-status-unset",
        }
    }

    /// The JSON object of `text`; empty when there is none or it is not one.
    fn object(text: Option<&str>) -> Map<String, Value> {
        text.and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default()
    }

    fn resource(text: Option<&str>) -> Vec<Pair> {
        let resource = Self::object(text);
        Self::RESOURCE_KEYS
            .into_iter()
            .filter_map(|key| {
                resource.get(key).map(|value| Pair {
                    key: key.to_owned(),
                    value: Text(value).to_string(),
                })
            })
            .collect()
    }

    /// The events of a span starting at `start`, as `{"name", "time_us", "attributes"}`
    /// objects in a JSON array (`otlp/map.rs`).
    fn events(trace: &Trace, start: MicroTimestamp, text: Option<&str>) -> Vec<SpanEvent> {
        let events: Vec<Value> = text
            .and_then(|t| serde_json::from_str(t).ok())
            .unwrap_or_default();
        events
            .into_iter()
            .filter_map(|event| {
                let Value::Object(mut event) = event else {
                    return None;
                };
                let name = event.get("name").map(|n| Text(n).to_string());
                let time = event.get("time_us").and_then(Value::as_i64);
                let time = MicroTimestamp::from_micros(time.unwrap_or(start.as_micros()));
                let in_span = time.as_micros().saturating_sub(start.as_micros());
                let in_span = Duration::from_micros(u64::try_from(in_span).unwrap_or(0));
                let mut attributes = match event.remove("attributes") {
                    Some(Value::Object(attributes)) => attributes,
                    _ => Map::new(),
                };
                let stacktrace = attributes
                    .remove("exception.stacktrace")
                    .map(|v| Text(&v).to_string());
                let name = name.unwrap_or_default();
                Some(SpanEvent {
                    at: format!(
                        "@ {} (+{} in span)",
                        Elapsed(trace.offset(time)),
                        Elapsed(in_span)
                    ),
                    exception: name == "exception",
                    name,
                    attributes: attributes
                        .iter()
                        .map(|(key, value)| Pair {
                            key: key.clone(),
                            value: Text(value).to_string(),
                        })
                        .collect(),
                    stacktrace,
                })
            })
            .collect()
    }
}

impl Attribute {
    /// Whether `key` and `value` tell of a failure.
    fn alerts(key: &str, value: &Value) -> bool {
        match key {
            "error.type" => true,
            "http.response.status_code" => {
                let code = match value {
                    Value::Number(n) => n.as_u64(),
                    Value::String(s) => s.trim().parse().ok(),
                    _ => None,
                };
                code.is_some_and(|code| (500..600).contains(&code))
            }
            _ => false,
        }
    }
}

impl SpanLog {
    /// The class of its level: its colour.
    pub fn level_class(&self) -> &'static str {
        match self.level {
            Some(Level::Error) => "sd-log-error",
            Some(Level::Warn) => "sd-log-warn",
            _ => "",
        }
    }
}

impl From<&LogEntry> for SpanLog {
    /// The record, linking to the Logs tab from it on: its stack (or hoster's records without
    /// one), filtered to its trace, from the second it was made, the page starting at the
    /// record.
    fn from(entry: &LogEntry) -> Self {
        let record = &entry.record;
        let time = TracesView::utc(record.time);
        let since = humantime::format_rfc3339_seconds(record.time.into()).to_string();
        // A page "after" a key holds the records past it: the key just before this record's.
        let before = RecordKey {
            time: record.time,
            id: entry.id.saturating_sub(1),
        };
        let stack = record.stack.as_deref().unwrap_or(LogScope::HOSTER);
        let trace = record.trace_id.map(|id| id.to_string());
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("stack", stack)
            .extend_pairs(trace.iter().map(|trace| ("trace", trace)))
            .append_pair("since", &since)
            .append_pair("after", &before.to_string())
            .finish();
        Self {
            shown: time[11..23].to_owned(),
            time,
            level: record.level,
            message: record.message.clone(),
            href: format!("{}?{query}", Telemetry::LOGS),
        }
    }
}

/// A JSON value as text: a string as it is, anything else as JSON.
struct Text<'a>(&'a Value);

impl fmt::Display for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::String(text) => f.write_str(text),
            other => write!(f, "{other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_in_red_are_error_types_and_5xx_status_codes() {
        let alerts = |key, value: Value| Attribute::alerts(key, &value);
        assert!(alerts("error.type", Value::from("timeout")));
        assert!(alerts("http.response.status_code", Value::from(503)));
        assert!(alerts("http.response.status_code", Value::from("500")));
        assert!(!alerts("http.response.status_code", Value::from(404)));
        assert!(!alerts("http.response.status_code", Value::from(600)));
        assert!(!alerts("db.system", Value::from("postgresql")));
    }

    #[test]
    fn values_are_strings_as_they_are_and_json_otherwise() {
        assert_eq!(Text(&Value::from("a \"b\"")).to_string(), "a \"b\"");
        assert_eq!(Text(&Value::from(3)).to_string(), "3");
        assert_eq!(Text(&serde_json::json!(["x", 1])).to_string(), r#"["x",1]"#);
        assert_eq!(Text(&Value::Null).to_string(), "null");
    }
}
