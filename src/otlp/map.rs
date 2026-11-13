//! From OTLP's protobuf types to the log store's rows (ADR-0069 §6).
//!
//! The stack and service of every row come from the verified token ([`Origin`]), never from the
//! payload. Attributes become one flat JSON object (`key: value`, with arrays and nested maps
//! kept as JSON, bytes as base64); a span's resource attributes are kept the same way, for
//! display. Times are the payload's, except that a time that is absent or later than the time
//! hoster received the export becomes the time received: retention deletes by these times, so
//! a time in the future would keep a row past it.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::logs::v1 as logs;
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1 as trace;
use serde_json::{Map, Value, json};

use crate::logs::{OtlpSeverity, ParsedLine};
use crate::secrets::ServiceIdentity;
use crate::store::{
    HexId, LogRecord, MicroTimestamp, Span, SpanKind, SpanStatus, Stream, Timestamp,
};

/// Who sent an export, by its token, and when hoster received it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Origin {
    stack: String,
    service: String,
    received: MicroTimestamp,
}

impl Origin {
    /// `identity`'s export, received at `received`.
    pub(super) fn received_at(identity: ServiceIdentity, received: MicroTimestamp) -> Self {
        Self {
            stack: identity.stack.to_string(),
            service: identity.service.to_string(),
            received,
        }
    }

    /// The export's log records, in the payload's order.
    pub(super) fn records(
        &self,
        export: ExportLogsServiceRequest,
    ) -> impl Iterator<Item = LogRecord> + '_ {
        export
            .resource_logs
            .into_iter()
            .flat_map(|resource| resource.scope_logs)
            .flat_map(|scope| scope.log_records)
            .map(|record| {
                LogRecord::from(Exported {
                    origin: self,
                    item: record,
                })
            })
    }

    /// The export's spans, in the payload's order, each with its resource's attributes; an
    /// [`InvalidSpan`] for each span that cannot be stored.
    pub(super) fn spans(
        &self,
        export: ExportTraceServiceRequest,
    ) -> impl Iterator<Item = Result<Span, InvalidSpan>> + '_ {
        export.resource_spans.into_iter().flat_map(move |resource| {
            let attributes = Option::<String>::from(Attributes::from(resource.resource));
            resource
                .scope_spans
                .into_iter()
                .flat_map(|scope| scope.spans)
                .map(move |span| {
                    Span::try_from(ExportedSpan {
                        origin: self,
                        resource: attributes.clone(),
                        span,
                    })
                })
        })
    }

    /// The payload's time in nanoseconds since the epoch, as stored: the time received when it
    /// is zero (absent) or later than that.
    fn time(&self, unix_nanos: u64) -> MicroTimestamp {
        match unix_nanos {
            0 => self.received,
            nanos => MicroTimestamp::from_unix_nanos(nanos).min(self.received),
        }
    }
}

/// A log record with the export it came in.
struct Exported<'a, T> {
    origin: &'a Origin,
    item: T,
}

/// A span with the export it came in and its resource's attributes as JSON text.
struct ExportedSpan<'a> {
    origin: &'a Origin,
    resource: Option<String>,
    span: trace::Span,
}

/// A span that cannot be stored: its trace or span id is not 16 or 8 bytes, or all zeros, or
/// its parent id is present but malformed. Counted as rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InvalidSpan;

impl From<Exported<'_, logs::LogRecord>> for LogRecord {
    /// The time is the record's, else the time its SDK observed it, else the time received.
    /// The body is the message (a string as it is, anything else as JSON, the event name when
    /// there is none), cut like a container's line; the attributes are the fields.
    fn from(exported: Exported<'_, logs::LogRecord>) -> Self {
        let Exported { origin, item } = exported;
        let nanos = match item.time_unix_nano {
            0 => item.observed_time_unix_nano,
            nanos => nanos,
        };
        let mut message = match item.body {
            Some(body) => String::from(Json::from(body)),
            None => item.event_name,
        };
        let cut = message.len() > ParsedLine::MAX_LEN;
        if cut {
            message.truncate(message.floor_char_boundary(ParsedLine::MAX_LEN));
        }
        let trace_id = Option::from(OtlpId(&item.trace_id));
        let span_id = Option::from(OtlpId(&item.span_id));
        Self {
            time: Timestamp::from(origin.time(nanos)),
            stack: Some(origin.stack.clone()),
            service: origin.service.clone(),
            generation: None,
            stream: Stream::Otlp,
            level: Option::from(OtlpSeverity {
                number: item.severity_number,
                text: &item.severity_text,
            }),
            message,
            fields: Option::from(Attributes::from(item.attributes)),
            cut,
            marker: false,
            trace_id,
            span_id,
            received: Some(Timestamp::from(origin.received)),
        }
    }
}

impl TryFrom<ExportedSpan<'_>> for Span {
    type Error = InvalidSpan;

    /// The start is clamped to the time received; the duration is the payload's end minus
    /// its start (zero when either is absent or the end is earlier).
    fn try_from(exported: ExportedSpan<'_>) -> Result<Self, Self::Error> {
        let ExportedSpan {
            origin,
            resource,
            span,
        } = exported;
        let trace_id = Option::from(OtlpId(&span.trace_id)).ok_or(InvalidSpan)?;
        let span_id = Option::from(OtlpId(&span.span_id)).ok_or(InvalidSpan)?;
        let parent = match span.parent_span_id.as_slice() {
            [] => None,
            bytes => Some(Option::from(OtlpId(bytes)).ok_or(InvalidSpan)?),
        };
        let duration = match (span.start_time_unix_nano, span.end_time_unix_nano) {
            (0, _) | (_, 0) => Duration::ZERO,
            (start, end) => Duration::from_nanos(end.saturating_sub(start)),
        };
        let (status, status_message) = match span.status {
            Some(status) => (
                SpanStatus::from(
                    trace::status::StatusCode::try_from(status.code)
                        .unwrap_or(trace::status::StatusCode::Unset),
                ),
                Some(status.message).filter(|message| !message.is_empty()),
            ),
            None => (SpanStatus::Unset, None),
        };
        let events: Vec<Value> = span
            .events
            .into_iter()
            .map(|event| Json::from(event).0)
            .collect();
        Ok(Self {
            trace_id,
            span_id,
            parent,
            stack: origin.stack.clone(),
            service: origin.service.clone(),
            name: span.name,
            kind: SpanKind::from(
                trace::span::SpanKind::try_from(span.kind)
                    .unwrap_or(trace::span::SpanKind::Unspecified),
            ),
            start: origin.time(span.start_time_unix_nano),
            duration,
            status,
            status_message,
            attributes: Option::from(Attributes::from(span.attributes)),
            resource,
            events: (!events.is_empty()).then(|| Value::Array(events).to_string()),
            received: Timestamp::from(origin.received),
        })
    }
}

impl From<trace::span::SpanKind> for SpanKind {
    fn from(kind: trace::span::SpanKind) -> Self {
        use trace::span::SpanKind as Otlp;
        match kind {
            Otlp::Unspecified => SpanKind::Unspecified,
            Otlp::Internal => SpanKind::Internal,
            Otlp::Server => SpanKind::Server,
            Otlp::Client => SpanKind::Client,
            Otlp::Producer => SpanKind::Producer,
            Otlp::Consumer => SpanKind::Consumer,
        }
    }
}

impl From<trace::status::StatusCode> for SpanStatus {
    fn from(code: trace::status::StatusCode) -> Self {
        use trace::status::StatusCode as Otlp;
        match code {
            Otlp::Unset => SpanStatus::Unset,
            Otlp::Ok => SpanStatus::Ok,
            Otlp::Error => SpanStatus::Error,
        }
    }
}

/// A trace or span id as OTLP carries it: bytes, empty when absent.
struct OtlpId<'a>(&'a [u8]);

/// `None` when absent, not `N` bytes long, or all zeros (W3C's invalid id).
impl<const N: usize> From<OtlpId<'_>> for Option<HexId<N>> {
    fn from(id: OtlpId<'_>) -> Self {
        if id.0.iter().all(|&byte| byte == 0) {
            return None;
        }
        HexId::try_from(id.0).ok()
    }
}

/// Attributes as one JSON object, `key: value`. A key given twice keeps its last value.
#[derive(Debug, Default)]
struct Attributes(Map<String, Value>);

impl From<Vec<KeyValue>> for Attributes {
    fn from(attributes: Vec<KeyValue>) -> Self {
        Self(
            attributes
                .into_iter()
                .map(|kv| (kv.key, kv.value.map_or(Value::Null, |v| Json::from(v).0)))
                .collect(),
        )
    }
}

impl From<Option<Resource>> for Attributes {
    fn from(resource: Option<Resource>) -> Self {
        resource.map_or_else(Self::default, |resource| Self::from(resource.attributes))
    }
}

/// The object as JSON text; `None` when it is empty.
impl From<Attributes> for Option<String> {
    fn from(attributes: Attributes) -> Self {
        (!attributes.0.is_empty()).then(|| Value::Object(attributes.0).to_string())
    }
}

/// An OTLP value, or a span event, as JSON.
#[derive(Debug, Clone, PartialEq)]
struct Json(Value);

impl From<AnyValue> for Json {
    /// Scalars as JSON scalars (a non-finite double as `null`), arrays and key-value lists as
    /// arrays and objects, bytes as base64 text as OTLP's JSON encoding writes them.
    fn from(value: AnyValue) -> Self {
        use any_value::Value as Otlp;
        Self(match value.value {
            None | Some(Otlp::StringValueStrindex(_)) => Value::Null,
            Some(Otlp::StringValue(text)) => Value::String(text),
            Some(Otlp::BoolValue(flag)) => Value::Bool(flag),
            Some(Otlp::IntValue(int)) => Value::from(int),
            Some(Otlp::DoubleValue(double)) => Value::from(double),
            Some(Otlp::ArrayValue(array)) => Value::Array(
                array
                    .values
                    .into_iter()
                    .map(|value| Json::from(value).0)
                    .collect(),
            ),
            Some(Otlp::KvlistValue(list)) => Value::Object(Attributes::from(list.values).0),
            Some(Otlp::BytesValue(bytes)) => Value::String(BASE64.encode(bytes)),
        })
    }
}

impl From<trace::span::Event> for Json {
    /// `{"name", "time_us", "attributes"}`, the attributes only when there are any. An event's
    /// time is kept as given: retention goes by the span's start.
    fn from(event: trace::span::Event) -> Self {
        let mut object = json!({
            "name": event.name,
            "time_us": MicroTimestamp::from_unix_nanos(event.time_unix_nano).as_micros(),
        });
        let attributes = Attributes::from(event.attributes);
        if !attributes.0.is_empty() {
            object["attributes"] = Value::Object(attributes.0);
        }
        Self(object)
    }
}

/// A log record's message: a string as it is, `null` as nothing, anything else as JSON text.
impl From<Json> for String {
    fn from(json: Json) -> Self {
        match json.0 {
            Value::String(text) => text,
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }
}
