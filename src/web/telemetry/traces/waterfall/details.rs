//! The selected span's details, under its row of the waterfall (epic qnh, as the mockup's span
//! details in `docs/design/Telemetry.dc.html`): what the span is, its timing, ids and status,
//! its attributes and resource, its events, and the log records made inside it.
//!
//! Everything comes from the [`Trace`] and its [`Span`] as stored, apart from the log records,
//! which are read once the span is known ([`SpanDetails::read_logs`]). The proxy's spans (the
//! service [`Span::EDGE_SERVICE`], built from the request row) have attributes and a resource
//! of `service.name` only, and no events, so their details say "no events on this span".
//!
//! The JSON columns are shown as the OTLP receiver keeps them: a string value as its text,
//! anything else (numbers, arrays, nested maps) as JSON. Keys are in order; templates escape
//! every value.

use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

use super::super::TraceParams;
use super::SpanTime;
use crate::otlp::ResourceAttributes;
use crate::store::{
    Level, LogEntry, LogStore, MicroTimestamp, RecordKey, Span, SpanId, SpanKind, SpanStatus,
    Trace, TraceId, TraceSpan,
};
use crate::web::logs::LogScope;
use crate::web::telemetry::Telemetry;

/// The details of the selected span.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanDetails {
    pub id: SpanId,
    pub name: String,
    pub service: String,
    pub kind: SpanKind,
    pub status: SpanStatus,
    /// The status pill's class: red in error, green for ok, neutral when unset.
    pub status_class: &'static str,
    /// Where closing the details goes.
    pub close: String,
    /// How long after the trace's start it started: `+51.7 ms`.
    pub start: String,
    pub duration: String,
    /// Its duration's share of the trace's (`94.1%`); `None` for a trace of no length.
    pub share: Option<String>,
    /// Its duration minus the union of its children's ([`TraceSpan::self_time`]).
    pub self_time: String,
    /// The parent's id, or `— (root)`.
    pub parent: String,
    /// Selecting the parent, when the trace has it.
    pub parent_href: Option<String>,
    /// It names a parent the trace does not have.
    pub parent_missing: bool,
    /// The status message of a span in error.
    pub message: Option<String>,
    pub attributes: Vec<Attribute>,
    /// The attributes as stored, when they are not a JSON object.
    pub attributes_raw: Option<String>,
    pub resource: Vec<ResourceChip>,
    pub events: Vec<SpanEvent>,
    pub logs: SpanLogs,
}

impl SpanDetails {
    /// What a root's parent says.
    pub const ROOT: &str = "— (root)";

    /// The details of `row` of `trace`, their links keeping what `params` has; no log records
    /// read yet. Their colour is the row's.
    pub(super) fn new(trace: &Trace, row: &TraceSpan, params: &TraceParams) -> Self {
        let span = row.span();
        let total = trace.duration();
        let share = (!total.is_zero()).then(|| {
            format!(
                "{:.1}%",
                span.duration.as_secs_f64() / total.as_secs_f64() * 100.0
            )
        });
        let parent_row = trace.parent_of(row).filter(|_| !row.parent_missing());
        let (attributes, attributes_raw) = Attribute::list(span.attributes.as_deref());
        Self {
            id: span.span_id,
            name: span.name.clone(),
            service: span.service.clone(),
            kind: span.kind,
            status: span.status,
            status_class: match span.status {
                SpanStatus::Error => "pill-fail",
                SpanStatus::Ok => "pill-ok",
                SpanStatus::Unset => "pill-neutral",
            },
            close: params.close_span(),
            start: format!("+{}", SpanTime(trace.offset(row))),
            duration: SpanTime(span.duration).to_string(),
            share,
            self_time: SpanTime(row.self_time()).to_string(),
            parent: span
                .parent
                .map_or_else(|| Self::ROOT.to_owned(), |id| id.to_string()),
            parent_href: parent_row.map(|parent| params.select_span(parent.span().span_id)),
            parent_missing: row.parent_missing(),
            message: span
                .is_error()
                .then(|| span.status_message.clone())
                .flatten(),
            attributes,
            attributes_raw,
            resource: ResourceChip::list(span.resource.as_deref()),
            events: SpanEvent::list(trace, span),
            logs: SpanLogs::default(),
        }
    }

    /// Reads the span's log records, of trace `trace`, from `store`.
    pub(super) async fn read_logs(&mut self, store: &LogStore, trace: TraceId) {
        self.logs = match store
            .span_records(trace, self.id, SpanLogs::LIMIT + 1)
            .await
        {
            Ok(entries) => SpanLogs::from(entries),
            Err(err) => {
                tracing::warn!(error = %err, "reading a span's log records failed");
                SpanLogs {
                    unreadable: true,
                    ..SpanLogs::default()
                }
            }
        };
    }
}

/// A JSON value as the details write it: a string as its text, anything else as JSON.
struct ValueText<'a>(&'a Value);

impl fmt::Display for ValueText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Value::String(text) => f.write_str(text),
            other => write!(f, "{other}"),
        }
    }
}

/// A JSON object column (attributes, resource), parsed.
struct ObjectColumn(Map<String, Value>);

/// `Err` with the text when it is not a JSON object.
impl TryFrom<&str> for ObjectColumn {
    type Error = String;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        serde_json::from_str(text)
            .map(Self)
            .map_err(|_| text.to_owned())
    }
}

/// One attribute of the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub key: String,
    pub value: String,
    /// Shown in red: `error.type`, or a 5xx `http.response.status_code`.
    pub alert: bool,
}

impl Attribute {
    /// The attributes of the column `text`, by key, or its text when it is not a JSON object.
    fn list(text: Option<&str>) -> (Vec<Self>, Option<String>) {
        match text.map(ObjectColumn::try_from) {
            None => (Vec::new(), None),
            Some(Err(raw)) => (Vec::new(), Some(raw)),
            Some(Ok(ObjectColumn(map))) => {
                let attributes = map
                    .iter()
                    .map(|(key, value)| Self {
                        alert: Self::alerts(key, value),
                        key: key.clone(),
                        value: ValueText(value).to_string(),
                    })
                    .collect();
                (attributes, None)
            }
        }
    }

    /// Whether `key` with `value` says what went wrong: `error.type`, or a 5xx status code,
    /// given as a number or as text.
    fn alerts(key: &str, value: &Value) -> bool {
        match key {
            "error.type" => true,
            "http.response.status_code" => {
                let code = match value {
                    Value::Number(n) => n.as_u64(),
                    Value::String(text) => text.trim().parse().ok(),
                    _ => None,
                };
                code.is_some_and(|code| (500..600).contains(&code))
            }
            _ => false,
        }
    }
}

/// One `key=value` chip of the span's resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceChip {
    pub key: String,
    pub value: String,
}

impl ResourceChip {
    /// The resource attributes shown, in this order, where the resource has them: the service
    /// and its version, the SDK's language, and hoster's stack and deployment generation, as
    /// it injects them (`OTEL_RESOURCE_ATTRIBUTES`).
    pub const KEYS: [&str; 5] = [
        "service.name",
        "service.version",
        "telemetry.sdk.language",
        ResourceAttributes::NAMESPACE,
        ResourceAttributes::GENERATION,
    ];

    /// The chips of the resource column `text`; none when it is absent or not an object.
    fn list(text: Option<&str>) -> Vec<Self> {
        let Some(Ok(ObjectColumn(map))) = text.map(ObjectColumn::try_from) else {
            return Vec::new();
        };
        Self::KEYS
            .iter()
            .filter_map(|key| {
                map.get(*key).map(|value| Self {
                    key: (*key).to_owned(),
                    value: ValueText(value).to_string(),
                })
            })
            .collect()
    }
}

/// One event of the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanEvent {
    pub name: String,
    /// When it happened: `@ 1.85 s (+1.80 s in span)`, from the trace's start and the span's.
    pub at: String,
    /// An `exception` event, shown in red.
    pub exception: bool,
    /// Its attributes by key, but the stack trace.
    pub attributes: Vec<(String, String)>,
    /// `exception.stacktrace`, shown as it is.
    pub stacktrace: Option<String>,
}

impl SpanEvent {
    /// The event name OpenTelemetry records an exception as.
    const EXCEPTION: &str = "exception";
    /// The attribute of an exception's stack trace.
    const STACKTRACE: &str = "exception.stacktrace";

    /// The events of `span` of `trace`, as stored; none when the column is absent or not a JSON
    /// array, and an entry that is not an object is skipped.
    fn list(trace: &Trace, span: &Span) -> Vec<Self> {
        let Some(Ok(Value::Array(events))) = span.events.as_deref().map(serde_json::from_str)
        else {
            return Vec::new();
        };
        let trace_start = trace.start().unwrap_or(span.start);
        events
            .iter()
            .filter_map(Value::as_object)
            .map(|event| {
                let name = event
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let time = event
                    .get("time_us")
                    .and_then(Value::as_i64)
                    .map_or(span.start, MicroTimestamp::from_micros);
                let since = |from: MicroTimestamp| {
                    Duration::from_micros(
                        u64::try_from(time.as_micros() - from.as_micros()).unwrap_or(0),
                    )
                };
                let mut attributes = Vec::new();
                let mut stacktrace = None;
                for (key, value) in event
                    .get("attributes")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    let text = ValueText(value).to_string();
                    if key == Self::STACKTRACE {
                        stacktrace = Some(text);
                    } else {
                        attributes.push((key.clone(), text));
                    }
                }
                Self {
                    at: format!(
                        "@ {} (+{} in span)",
                        SpanTime(since(trace_start)),
                        SpanTime(since(span.start))
                    ),
                    exception: name == Self::EXCEPTION,
                    name,
                    attributes,
                    stacktrace,
                }
            })
            .collect()
    }
}

/// The log records made inside the span, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpanLogs {
    /// At most [`SpanLogs::LIMIT`].
    pub records: Vec<SpanLog>,
    /// The span has more: the Logs tab from the first record not shown.
    pub more: Option<String>,
    /// The store could not be read.
    pub unreadable: bool,
}

impl SpanLogs {
    /// The most records shown.
    pub const LIMIT: u32 = 50;

    /// `the first 50 records`, when the span has more.
    pub fn first(&self) -> String {
        format!("the first {} records", Self::LIMIT)
    }
}

/// Records read with a limit of one more than [`SpanLogs::LIMIT`], oldest first, so one past
/// the limit says there are more.
impl From<Vec<LogEntry>> for SpanLogs {
    fn from(mut entries: Vec<LogEntry>) -> Self {
        let limit = usize::try_from(Self::LIMIT).unwrap_or(usize::MAX);
        let more = entries.get(limit).map(|next| SpanLog::from(next).href);
        entries.truncate(limit);
        Self {
            records: entries.iter().map(SpanLog::from).collect(),
            more,
            unreadable: false,
        }
    }
}

/// One log record made inside the span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanLog {
    /// RFC 3339 in UTC, to the millisecond.
    pub time: String,
    /// The time of day: `19:58:42.933`.
    pub shown: String,
    pub level: Option<Level>,
    pub message: String,
    /// The line was cut at the store's length cap.
    pub cut: bool,
    /// The record in the Logs tab.
    pub href: String,
}

/// The Logs tab opened on a record: its stack (or hoster's own records, for one without), the
/// range from its time, and the page of records after the one before it, so it is the page's
/// first. The Logs tab has no filter by trace yet; one by `trace` can narrow this link.
impl From<&LogEntry> for SpanLog {
    fn from(entry: &LogEntry) -> Self {
        let record = &entry.record;
        let time = humantime::format_rfc3339_millis(record.time.into()).to_string();
        let before = RecordKey {
            time: record.time,
            id: entry.id - 1,
        }
        .to_string();
        let query = form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("stack", record.stack.as_deref().unwrap_or(LogScope::HOSTER)),
                ("since", &time),
                ("after", &before),
            ])
            .finish();
        Self {
            shown: time.get(11..23).unwrap_or(&time).to_owned(),
            href: format!("{}?{query}", Telemetry::LOGS),
            time,
            level: record.level,
            message: record.message.clone(),
            cut: record.cut,
        }
    }
}
