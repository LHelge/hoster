//! The OTLP receiver: the mapping from payloads built with the proto types, the handlers through
//! the router (auth, encodings, limits, partial success), and one round trip with the
//! OpenTelemetry SDK's own OTLP/HTTP exporters.

use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use flate2::Compression;
use flate2::write::GzEncoder;
use http_body_util::BodyExt as _;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{
    AnyValue, ArrayValue, KeyValue, KeyValueList, any_value,
};
use opentelemetry_proto::tonic::logs::v1::{self as logs, ResourceLogs, ScopeLogs};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{self as trace, ResourceSpans, ScopeSpans};
use prost::Message as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::map::Origin;
use super::{OtlpReceiver, RpcStatus};
use crate::logs::{LogSink, Tap};
use crate::secrets::{KnownServices, SecretKey, ServiceIdentity, TokenKey};
use crate::stack::Name;
use crate::store::{
    Level, LogRecord, MicroTimestamp, Span, SpanKind, SpanStatus, Stream, Timestamp, TraceId,
};

const TRACE: [u8; 16] = [0xab; 16];
const ROOT: [u8; 8] = [0x01; 8];
const CHILD: [u8; 8] = [0x02; 8];
/// 2026-11-13T12:00:00Z, in nanoseconds since the epoch.
const NOON: u64 = 1_794_571_200_000_000_000;

fn name(s: &str) -> Name {
    s.parse().unwrap()
}

/// The services that exist: `shop/web` and `shop/worker`.
struct Known(BTreeSet<(Name, Name)>);

impl KnownServices for Known {
    fn contains(&self, stack: &Name, service: &Name) -> bool {
        self.0.contains(&(stack.clone(), service.clone()))
    }
}

/// A receiver over a tapped sink of `capacity`.
struct Fixture {
    key: TokenKey,
    receiver: OtlpReceiver,
    tap: Tap,
}

impl Fixture {
    fn new(capacity: usize) -> Self {
        Self::with_key(TokenKey::from(&SecretKey::generate()), capacity)
    }

    fn with_key(key: TokenKey, capacity: usize) -> Self {
        let known = Known(
            [("shop", "web"), ("shop", "worker")]
                .into_iter()
                .map(|(s, v)| (name(s), name(v)))
                .collect(),
        );
        let (sink, tap) = LogSink::tapped(capacity);
        let receiver = OtlpReceiver::new(key.clone(), Arc::new(known), sink);
        Self { key, receiver, tap }
    }

    fn bearer(&self, stack: &str, service: &str) -> String {
        format!("Bearer {}", self.key.token(&name(stack), &name(service)))
    }

    /// `POST path` with `headers` and `body`.
    async fn post(&self, path: &str, headers: &[(&str, &str)], body: Vec<u8>) -> Answer {
        let mut request = Request::post(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.send(request.body(Body::from(body)).unwrap()).await
    }

    /// `POST path` as `shop/web`, with a protobuf body.
    async fn export(&self, path: &str, body: Vec<u8>) -> Answer {
        let bearer = self.bearer("shop", "web");
        self.post(
            path,
            &[
                ("authorization", &bearer),
                ("content-type", "application/x-protobuf"),
            ],
            body,
        )
        .await
    }

    async fn send(&self, request: Request<Body>) -> Answer {
        let response = self.receiver.router().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Answer {
            status,
            headers,
            body,
        }
    }
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Answer {
    fn logs(&self) -> ExportLogsServiceResponse {
        assert_eq!(self.status, StatusCode::OK);
        assert_eq!(self.headers[header::CONTENT_TYPE], "application/x-protobuf");
        ExportLogsServiceResponse::decode(self.body.clone()).unwrap()
    }

    fn traces(&self) -> ExportTraceServiceResponse {
        assert_eq!(self.status, StatusCode::OK);
        assert_eq!(self.headers[header::CONTENT_TYPE], "application/x-protobuf");
        ExportTraceServiceResponse::decode(self.body.clone()).unwrap()
    }

    /// The `google.rpc.Status` of an error.
    fn error(&self) -> RpcStatus {
        assert_eq!(self.headers[header::CONTENT_TYPE], "application/x-protobuf");
        RpcStatus::decode(self.body.clone()).unwrap()
    }
}

fn string(text: &str) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::StringValue(text.to_owned())),
    }
}

fn int(value: i64) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::IntValue(value)),
    }
}

fn kv(key: &str, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.to_owned(),
        value: Some(value),
        ..KeyValue::default()
    }
}

/// A resource that claims to be another stack's service.
fn resource() -> Option<Resource> {
    Some(Resource {
        attributes: vec![
            kv("service.name", string("billing")),
            kv("hoster.stack", string("bank")),
        ],
        ..Resource::default()
    })
}

fn logs_export(records: Vec<logs::LogRecord>) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: resource(),
            scope_logs: vec![ScopeLogs {
                log_records: records,
                ..ScopeLogs::default()
            }],
            ..ResourceLogs::default()
        }],
    }
}

fn traces_export(spans: Vec<trace::Span>) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: resource(),
            scope_spans: vec![ScopeSpans {
                spans,
                ..ScopeSpans::default()
            }],
            ..ResourceSpans::default()
        }],
    }
}

fn record(message: &str) -> logs::LogRecord {
    logs::LogRecord {
        time_unix_nano: NOON,
        severity_number: 13,
        severity_text: "WARN".to_owned(),
        body: Some(string(message)),
        attributes: vec![kv("user.id", int(42))],
        trace_id: TRACE.to_vec(),
        span_id: CHILD.to_vec(),
        ..logs::LogRecord::default()
    }
}

fn span(id: [u8; 8], parent: Option<[u8; 8]>) -> trace::Span {
    trace::Span {
        trace_id: TRACE.to_vec(),
        span_id: id.to_vec(),
        parent_span_id: parent.map(|p| p.to_vec()).unwrap_or_default(),
        name: "GET /claims".to_owned(),
        kind: trace::span::SpanKind::Server as i32,
        start_time_unix_nano: NOON,
        end_time_unix_nano: NOON + 1_500_000,
        attributes: vec![kv("http.route", string("/claims"))],
        events: vec![trace::span::Event {
            time_unix_nano: NOON + 1_000_000,
            name: "exception".to_owned(),
            attributes: vec![kv("exception.message", string("boom"))],
            ..trace::span::Event::default()
        }],
        status: Some(trace::Status {
            message: "failed".to_owned(),
            code: trace::status::StatusCode::Error as i32,
        }),
        ..trace::Span::default()
    }
}

/// An origin for `shop/web` that received its export a day after [`NOON`].
fn origin() -> Origin {
    Origin::received_at(
        ServiceIdentity {
            stack: name("shop"),
            service: name("web"),
        },
        MicroTimestamp::from_unix_nanos(NOON) + Duration::from_secs(86_400),
    )
}

fn json(text: Option<&str>) -> Value {
    serde_json::from_str(text.expect("JSON text")).unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

// --- The mapping ---------------------------------------------------------------------------

#[test]
fn a_log_record_maps_to_the_tokens_service_with_its_level_ids_and_fields() {
    let origin = origin();
    let records: Vec<LogRecord> = origin.records(logs_export(vec![record("slow")])).collect();
    let [record] = records.as_slice() else {
        panic!("{records:?}")
    };
    assert_eq!(record.stack.as_deref(), Some("shop"));
    assert_eq!(record.service, "web");
    assert_eq!(record.stream, Stream::Otlp);
    assert_eq!(record.level, Some(Level::Warn));
    assert_eq!(record.message, "slow");
    assert_eq!(json(record.fields.as_deref()), json!({"user.id": 42}));
    assert_eq!(record.trace_id, Some(TraceId::from(TRACE)));
    assert_eq!(record.span_id.unwrap().to_string(), "0202020202020202");
    assert_eq!(
        record.time,
        Timestamp::from(MicroTimestamp::from_unix_nanos(NOON))
    );
    assert_eq!(record.received, Some(Timestamp::from(origin_received())));
    assert!(!record.cut && !record.marker);
    assert_eq!(record.generation, None);
}

fn origin_received() -> MicroTimestamp {
    MicroTimestamp::from_unix_nanos(NOON) + Duration::from_secs(86_400)
}

#[test]
fn a_structured_body_is_json_and_attribute_values_keep_their_shape() {
    let body = AnyValue {
        value: Some(any_value::Value::KvlistValue(KeyValueList {
            values: vec![kv("order", int(7)), kv("ok", string("yes"))],
        })),
    };
    let mut item = record("");
    item.body = Some(body);
    item.attributes = vec![
        kv(
            "list",
            AnyValue {
                value: Some(any_value::Value::ArrayValue(ArrayValue {
                    values: vec![int(1), string("two")],
                })),
            },
        ),
        kv(
            "raw",
            AnyValue {
                value: Some(any_value::Value::BytesValue(vec![0xff, 0x00])),
            },
        ),
        kv(
            "ratio",
            AnyValue {
                value: Some(any_value::Value::DoubleValue(0.5)),
            },
        ),
        kv(
            "flag",
            AnyValue {
                value: Some(any_value::Value::BoolValue(true)),
            },
        ),
        kv("empty", AnyValue { value: None }),
        kv("user.id", int(1)),
        kv("user.id", int(2)),
    ];
    let record = origin().records(logs_export(vec![item])).next().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&record.message).unwrap(),
        json!({"order": 7, "ok": "yes"})
    );
    assert_eq!(
        json(record.fields.as_deref()),
        json!({
            "list": [1, "two"],
            "raw": "/wA=",
            "ratio": 0.5,
            "flag": true,
            "empty": null,
            "user.id": 2,
        })
    );
}

#[test]
fn a_record_without_body_ids_or_level_keeps_what_it_has() {
    let item = logs::LogRecord {
        time_unix_nano: NOON,
        event_name: "user.login".to_owned(),
        trace_id: vec![0; 16],
        span_id: vec![1, 2, 3],
        ..logs::LogRecord::default()
    };
    let record = origin().records(logs_export(vec![item])).next().unwrap();
    assert_eq!(record.message, "user.login");
    assert_eq!(record.level, None);
    assert_eq!(record.fields, None);
    assert_eq!(record.trace_id, None, "all zeros is no trace id");
    assert_eq!(record.span_id, None, "three bytes is no span id");
}

#[test]
fn a_long_message_is_cut_like_a_containers_line() {
    let long = "é".repeat(crate::logs::ParsedLine::MAX_LEN);
    let record = origin()
        .records(logs_export(vec![record(&long)]))
        .next()
        .unwrap();
    assert!(record.cut);
    assert!(record.message.len() <= crate::logs::ParsedLine::MAX_LEN);
    assert!(long.starts_with(&record.message));
}

#[test]
fn times_in_the_future_or_absent_become_the_time_received() {
    let origin = origin();
    let received = origin_received();
    let later = NOON + 2 * 86_400 * 1_000_000_000;

    // A record: its own time, else the observed time, else the time received; never later.
    let times = |time_unix_nano, observed_time_unix_nano| {
        let item = logs::LogRecord {
            time_unix_nano,
            observed_time_unix_nano,
            ..record("x")
        };
        origin.records(logs_export(vec![item])).next().unwrap().time
    };
    let noon = Timestamp::from(MicroTimestamp::from_unix_nanos(NOON));
    assert_eq!(times(NOON, 0), noon);
    assert_eq!(times(0, NOON), noon);
    assert_eq!(times(0, 0), Timestamp::from(received));
    assert_eq!(times(later, 0), Timestamp::from(received));
    assert_eq!(times(0, later), Timestamp::from(received));

    // A span: its start, else the time received; never later. The duration stays the
    // payload's.
    let start = |start_time_unix_nano, end_time_unix_nano| {
        let item = trace::Span {
            start_time_unix_nano,
            end_time_unix_nano,
            ..span(ROOT, None)
        };
        let span = origin
            .spans(traces_export(vec![item]))
            .next()
            .unwrap()
            .unwrap();
        (span.start, span.duration)
    };
    assert_eq!(
        start(NOON, NOON + 3_000),
        (
            MicroTimestamp::from_unix_nanos(NOON),
            Duration::from_nanos(3_000)
        )
    );
    assert_eq!(
        start(later, later + 2_000_000),
        (received, Duration::from_millis(2))
    );
    assert_eq!(start(0, NOON), (received, Duration::ZERO));
    assert_eq!(
        start(NOON, 0),
        (MicroTimestamp::from_unix_nanos(NOON), Duration::ZERO)
    );
    assert_eq!(
        start(NOON, NOON - 1),
        (MicroTimestamp::from_unix_nanos(NOON), Duration::ZERO)
    );
}

#[test]
fn a_span_maps_with_its_resource_events_kind_and_status() {
    let origin = origin();
    let spans: Vec<Span> = origin
        .spans(traces_export(vec![
            span(ROOT, None),
            span(CHILD, Some(ROOT)),
        ]))
        .collect::<Result<_, _>>()
        .unwrap();
    let [root, child] = spans.as_slice() else {
        panic!("{spans:?}")
    };
    assert_eq!(root.parent, None);
    assert_eq!(child.parent.unwrap().to_string(), "0101010101010101");
    assert_eq!(child.span_id.to_string(), "0202020202020202");
    assert_eq!(child.trace_id, TraceId::from(TRACE));
    assert_eq!(
        (child.stack.as_str(), child.service.as_str()),
        ("shop", "web")
    );
    assert_eq!(child.name, "GET /claims");
    assert_eq!(child.kind, SpanKind::Server);
    assert_eq!(child.status, SpanStatus::Error);
    assert_eq!(child.status_message.as_deref(), Some("failed"));
    assert_eq!(child.duration, Duration::from_micros(1_500));
    assert_eq!(
        json(child.attributes.as_deref()),
        json!({"http.route": "/claims"})
    );
    // The payload's claims are kept for display only.
    assert_eq!(
        json(child.resource.as_deref()),
        json!({"service.name": "billing", "hoster.stack": "bank"})
    );
    let noon_us = MicroTimestamp::from_unix_nanos(NOON).as_micros();
    assert_eq!(
        json(child.events.as_deref()),
        json!([{
            "name": "exception",
            "time_us": noon_us + 1_000,
            "attributes": {"exception.message": "boom"},
        }])
    );
    assert_eq!(child.received, Timestamp::from(origin_received()));
}

#[test]
fn a_bare_span_has_no_json_columns_and_unknown_enums_are_unspecified() {
    let item = trace::Span {
        attributes: Vec::new(),
        events: Vec::new(),
        status: None,
        kind: 99,
        ..span(ROOT, None)
    };
    let mut export = traces_export(vec![item]);
    export.resource_spans[0].resource = None;
    let span = origin().spans(export).next().unwrap().unwrap();
    assert_eq!(span.kind, SpanKind::Unspecified);
    assert_eq!(span.status, SpanStatus::Unset);
    assert_eq!(span.status_message, None);
    assert_eq!(
        (span.attributes, span.resource, span.events),
        (None, None, None)
    );
}

#[test]
fn spans_with_invalid_ids_are_rejected() {
    let bad = [
        trace::Span {
            trace_id: Vec::new(),
            ..span(ROOT, None)
        },
        trace::Span {
            trace_id: vec![0; 16],
            ..span(ROOT, None)
        },
        trace::Span {
            span_id: vec![1; 4],
            ..span(ROOT, None)
        },
        trace::Span {
            parent_span_id: vec![1; 9],
            ..span(ROOT, None)
        },
    ];
    for item in bad {
        let mapped = origin().spans(traces_export(vec![item.clone()])).next();
        assert!(matches!(mapped, Some(Err(_))), "{item:?}");
    }
}

// --- The handlers --------------------------------------------------------------------------

#[tokio::test]
async fn logs_are_accepted_and_offered_under_the_tokens_service() {
    let mut fixture = Fixture::new(16);
    let export = logs_export(vec![record("one"), record("two")]);
    let response = fixture
        .export("/v1/logs", export.encode_to_vec())
        .await
        .logs();
    assert_eq!(response.partial_success, None);
    let records = fixture.tap.drain();
    assert_eq!(records.len(), 2);
    for (record, cursor) in &records {
        assert_eq!(cursor, &None);
        assert_eq!(
            (record.stack.as_deref(), record.service.as_str()),
            (Some("shop"), "web")
        );
        assert_eq!(record.stream, Stream::Otlp);
    }
    assert_eq!(records[1].0.message, "two");
}

#[tokio::test]
async fn traces_are_accepted_gzipped_or_not() {
    let mut fixture = Fixture::new(16);
    let export = traces_export(vec![span(ROOT, None), span(CHILD, Some(ROOT))]);
    let response = fixture
        .export("/v1/traces", export.encode_to_vec())
        .await
        .traces();
    assert_eq!(response.partial_success, None);
    assert_eq!(fixture.tap.drain_spans().len(), 2);

    let bearer = fixture.bearer("shop", "worker");
    let response = fixture
        .post(
            "/v1/traces",
            &[
                ("authorization", &bearer),
                ("content-type", "application/x-protobuf"),
                ("content-encoding", "gzip"),
            ],
            gzip(&export.encode_to_vec()),
        )
        .await
        .traces();
    assert_eq!(response.partial_success, None);
    let spans = fixture.tap.drain_spans();
    assert_eq!(spans.len(), 2);
    assert!(spans.iter().all(|span| span.service == "worker"));
}

#[tokio::test]
async fn gzipped_logs_are_accepted() {
    let mut fixture = Fixture::new(16);
    let bearer = fixture.bearer("shop", "web");
    let body = gzip(&logs_export(vec![record("zipped")]).encode_to_vec());
    let response = fixture
        .post(
            "/v1/logs",
            &[
                ("authorization", &bearer),
                ("content-type", "Application/X-Protobuf; charset=binary"),
                ("content-encoding", "GZIP"),
            ],
            body,
        )
        .await
        .logs();
    assert_eq!(response.partial_success, None);
    assert_eq!(fixture.tap.drain()[0].0.message, "zipped");
}

#[tokio::test]
async fn missing_malformed_or_unknown_tokens_are_401_without_detail() {
    let mut fixture = Fixture::new(16);
    let other_key = TokenKey::from(&SecretKey::generate());
    let good = fixture.key.token(&name("shop"), &name("web")).to_string();
    let mac = good.rsplit_once('.').unwrap().1.to_owned();
    let body = logs_export(vec![record("x")]).encode_to_vec();
    for authorization in [
        None,
        Some(String::new()),
        Some(format!("Basic {good}")),
        Some("Bearer".to_owned()),
        Some("Bearer ".to_owned()),
        Some("Bearer not-a-token".to_owned()),
        Some(format!("Bearer shop.worker.{mac}")),
        Some(format!("Bearer shop.web.{}", &mac[1..])),
        Some(format!(
            "Bearer {}",
            fixture.key.token(&name("shop"), &name("gone"))
        )),
        Some(format!(
            "Bearer {}",
            fixture.key.token(&name("bank"), &name("web"))
        )),
        Some(format!(
            "Bearer {}",
            other_key.token(&name("shop"), &name("web"))
        )),
    ] {
        let mut headers = vec![("content-type", "application/x-protobuf")];
        if let Some(value) = &authorization {
            headers.push(("authorization", value));
        }
        let answer = fixture.post("/v1/logs", &headers, body.clone()).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{authorization:?}");
        let status = answer.error();
        assert_eq!(status.code, RpcStatus::UNAUTHENTICATED);
        assert_eq!(status.message, "", "{authorization:?}");
    }
    assert!(fixture.tap.drain().is_empty());
}

#[tokio::test]
async fn the_token_is_checked_before_the_body() {
    let fixture = Fixture::new(16);
    let answer = fixture
        .post(
            "/v1/traces",
            &[("content-type", "application/json")],
            b"{}".to_vec(),
        )
        .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn other_content_types_and_encodings_are_415() {
    let fixture = Fixture::new(16);
    let bearer = fixture.bearer("shop", "web");
    let body = traces_export(vec![span(ROOT, None)]).encode_to_vec();
    for headers in [
        vec![],
        vec![("content-type", "application/json")],
        vec![("content-type", "application/protobuf")],
        vec![("content-type", "application/grpc")],
        vec![
            ("content-type", "application/x-protobuf"),
            ("content-encoding", "br"),
        ],
        vec![
            ("content-type", "application/x-protobuf"),
            ("content-encoding", "gzip, deflate"),
        ],
    ] {
        let mut all = vec![("authorization", bearer.as_str())];
        all.extend(headers.iter().copied());
        let answer = fixture.post("/v1/traces", &all, body.clone()).await;
        assert_eq!(
            answer.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{headers:?}"
        );
        assert_eq!(answer.error().code, RpcStatus::INVALID_ARGUMENT);
    }
}

#[tokio::test]
async fn bodies_over_the_limit_are_413_before_and_after_inflating() {
    let fixture = Fixture::new(16);
    let bearer = fixture.bearer("shop", "web");
    let protobuf = [
        ("authorization", bearer.as_str()),
        ("content-type", "application/x-protobuf"),
    ];
    let answer = fixture
        .post("/v1/logs", &protobuf, vec![0; OtlpReceiver::MAX_BODY + 1])
        .await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);

    let gzipped = [
        ("authorization", bearer.as_str()),
        ("content-type", "application/x-protobuf"),
        ("content-encoding", "gzip"),
    ];
    let bomb = gzip(&vec![0; OtlpReceiver::MAX_DECODED + 1]);
    assert!(bomb.len() < OtlpReceiver::MAX_BODY);
    let answer = fixture.post("/v1/logs", &gzipped, bomb).await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(answer.error().code, RpcStatus::RESOURCE_EXHAUSTED);

    // Exactly at the limit inflates (and then fails to decode as an export).
    let full = gzip(&vec![0; OtlpReceiver::MAX_DECODED]);
    let answer = fixture.post("/v1/logs", &gzipped, full).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn malformed_bodies_are_400() {
    let mut fixture = Fixture::new(16);
    let bearer = fixture.bearer("shop", "web");
    let plain = [
        ("authorization", bearer.as_str()),
        ("content-type", "application/x-protobuf"),
    ];
    let gzipped = [
        ("authorization", bearer.as_str()),
        ("content-type", "application/x-protobuf"),
        ("content-encoding", "gzip"),
    ];
    for (headers, body) in [
        (&plain[..], vec![0xff, 0xff, 0xff]),
        (&plain[..], b"{\"resourceLogs\":[]}".to_vec()),
        (&gzipped[..], b"not gzip".to_vec()),
        (&gzipped[..], gzip(&[0xff, 0xff, 0xff])),
    ] {
        let answer = fixture.post("/v1/logs", headers, body).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        let status = answer.error();
        assert_eq!(status.code, RpcStatus::INVALID_ARGUMENT);
        assert_eq!(status.message, "the body is not an OTLP export request");
    }
    assert!(fixture.tap.drain().is_empty());
}

#[tokio::test]
async fn an_empty_export_is_a_success() {
    let fixture = Fixture::new(16);
    let response = fixture.export("/v1/logs", Vec::new()).await.logs();
    assert_eq!(response.partial_success, None);
    let response = fixture.export("/v1/traces", Vec::new()).await.traces();
    assert_eq!(response.partial_success, None);
}

#[tokio::test]
async fn what_the_sink_drops_is_reported_as_partial_success() {
    let mut fixture = Fixture::new(2);
    let records = (0..5).map(|n| record(&n.to_string())).collect();
    let response = fixture
        .export("/v1/logs", logs_export(records).encode_to_vec())
        .await
        .logs();
    let partial = response.partial_success.expect("partial success");
    assert_eq!(partial.rejected_log_records, 3);
    assert_eq!(partial.error_message, "3 dropped: the log store is busy");
    assert_eq!(fixture.tap.drain().len(), 2);

    // Spans: one invalid, then the channel takes two of the three valid ones.
    let spans = vec![
        span(ROOT, None),
        trace::Span {
            trace_id: Vec::new(),
            ..span(CHILD, None)
        },
        span(CHILD, Some(ROOT)),
        span([3; 8], Some(ROOT)),
    ];
    let response = fixture
        .export("/v1/traces", traces_export(spans).encode_to_vec())
        .await
        .traces();
    let partial = response.partial_success.expect("partial success");
    assert_eq!(partial.rejected_spans, 2);
    assert_eq!(
        partial.error_message,
        "1 dropped: the log store is busy; 1 without a valid trace or span id"
    );
    assert_eq!(fixture.tap.drain_spans().len(), 2);
}

#[tokio::test]
async fn metrics_are_404_and_other_methods_405() {
    let fixture = Fixture::new(16);
    let answer = fixture.export("/v1/metrics", Vec::<u8>::new()).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(answer.error().code, RpcStatus::UNIMPLEMENTED);

    let answer = fixture
        .send(Request::get("/v1/logs").body(Body::empty()).unwrap())
        .await;
    assert_eq!(answer.status, StatusCode::METHOD_NOT_ALLOWED);
}

// --- A real exporter -----------------------------------------------------------------------

/// The SDK's OTLP/HTTP protobuf exporters, as an instrumented app configures them from the
/// injected environment, export a trace and a log record that land under the token's service.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sdks_otlp_http_exporters_round_trip() {
    use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _, Severity};
    use opentelemetry::trace::{TraceContextExt as _, Tracer as _, TracerProvider as _};
    use opentelemetry_otlp::WithHttpConfig as _;
    use opentelemetry_otlp::{LogExporter, Protocol, SpanExporter, WithExportConfig as _};
    use opentelemetry_sdk::Resource as SdkResource;
    use opentelemetry_sdk::logs::SdkLoggerProvider;
    use opentelemetry_sdk::trace::SdkTracerProvider;

    // The exporters build reqwest clients, which need a crypto provider even for plain HTTP.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // One receiver for each signal, as the tap hands out one kind of entry at a time.
    let mut traces = Fixture::new(64);
    let mut logs = Fixture::with_key(traces.key.clone(), 64);
    let mut addresses = Vec::new();
    let mut servers = Vec::new();
    for fixture in [&traces, &logs] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        addresses.push(listener.local_addr().unwrap());
        let router = fixture.receiver.router();
        servers.push(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    }
    let (traces_at, logs_at) = (addresses[0], addresses[1]);
    let headers = HashMap::from([("authorization".to_owned(), traces.bearer("shop", "web"))]);

    // The SDK's simple processors export synchronously, blocking: off the async workers.
    tokio::task::spawn_blocking(move || {
        let resource = SdkResource::builder().with_service_name("claims").build();
        let spans = SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(format!("http://{traces_at}/v1/traces"))
            .with_headers(headers.clone())
            .build()
            .unwrap();
        let tracer_provider = SdkTracerProvider::builder()
            .with_simple_exporter(spans)
            .with_resource(resource.clone())
            .build();
        let records = LogExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(format!("http://{logs_at}/v1/logs"))
            .with_headers(headers)
            .build()
            .unwrap();
        let logger_provider = SdkLoggerProvider::builder()
            .with_simple_exporter(records)
            .with_resource(resource)
            .build();

        let tracer = tracer_provider.tracer("round-trip");
        let logger = logger_provider.logger("round-trip");
        tracer.in_span("handle", |cx| {
            let context = cx.span().span_context().clone();
            tracer.in_span("query", |_| {});
            let mut record = logger.create_log_record();
            record.set_body("inside".into());
            record.set_severity_number(Severity::Error);
            record.set_trace_context(context.trace_id(), context.span_id(), None);
            logger.emit(record);
            cx.span().add_event("checkpoint", Vec::new());
        });
        tracer_provider.shutdown().unwrap();
        logger_provider.shutdown().unwrap();
    })
    .await
    .unwrap();
    for server in servers {
        server.abort();
    }

    let spans = traces.tap.drain_spans();
    let [query, handle] = spans.as_slice() else {
        panic!("{spans:?}")
    };
    assert_eq!(
        (query.name.as_str(), handle.name.as_str()),
        ("query", "handle")
    );
    assert_eq!(query.parent, Some(handle.span_id));
    assert_eq!(query.trace_id, handle.trace_id);
    assert_eq!(handle.parent, None);
    assert_eq!(
        (handle.stack.as_str(), handle.service.as_str()),
        ("shop", "web")
    );
    assert_eq!(json(handle.resource.as_deref())["service.name"], "claims");
    assert_eq!(json(handle.events.as_deref())[0]["name"], "checkpoint");

    let records = logs.tap.drain();
    let [(record, None)] = records.as_slice() else {
        panic!("{records:?}")
    };
    assert_eq!(record.message, "inside");
    assert_eq!(record.level, Some(Level::Error));
    assert_eq!(record.trace_id, Some(handle.trace_id));
    assert_eq!(record.span_id, Some(handle.span_id));
    assert_eq!(record.service, "web");
}
