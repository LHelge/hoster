//! The OTLP receiver (ADR-0069 §1, §2, §4, §6): `POST /v1/logs` and `POST /v1/traces` over
//! OTLP/HTTP with protobuf bodies, from containers' OpenTelemetry SDKs.
//!
//! [`OtlpReceiver::router`] builds the routes; which listener serves them, and the peer check in
//! front of them, belong to the proxy's `:80` (ADR-0069 §3). A request goes through:
//!
//! 1. **Identity**: `Authorization: Bearer <stack>.<service>.<mac>` is verified as a
//!    [`ServiceToken`] (MAC recomputed and compared in constant time, stack and service must
//!    exist). Anything else is `401` with no detail. The token decides where data is stored;
//!    what the payload claims (`service.name` and other resource attributes) is kept for display
//!    only. Neither the token nor the header is ever logged.
//! 2. **Encoding**: `Content-Type: application/x-protobuf`, with `Content-Encoding` absent,
//!    `identity` or `gzip`; anything else is `415` (no JSON encoding, no gRPC).
//! 3. **Body**: at most [`OtlpReceiver::MAX_BODY`] bytes as sent and
//!    [`OtlpReceiver::MAX_DECODED`] once inflated, else `413`; a body that does not inflate or
//!    decode as the export request is `400`.
//! 4. **Delivery**: the payload is mapped to [`LogRecord`](crate::store::LogRecord)s and
//!    [`Span`](crate::store::Span)s (see `map.rs`) and offered to the [`LogSink`], which never
//!    waits for the store. What the sink drops (its channel is full) and spans with invalid ids
//!    are answered as OTLP `partial_success` with the rejected count, so SDKs do not retry them.
//!
//! Errors carry a `google.rpc.Status` body, as the OTLP/HTTP specification asks, with a fixed
//! message that never echoes the request. `/v1/metrics` is `404` (ADR-0069 §2).

mod map;
#[cfg(test)]
mod tests;

use std::io::Read as _;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest as _, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use flate2::read::GzDecoder;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsPartialSuccess, ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTracePartialSuccess, ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use prost::Message;

use self::map::{InvalidSpan, Origin};
use crate::logs::LogSink;
use crate::secrets::{KnownServices, ServiceIdentity, ServiceToken, TokenKey};

/// The OTLP receiver's collaborators. Cheap to clone.
#[derive(Clone)]
pub struct OtlpReceiver(Arc<Inner>);

struct Inner {
    /// Verifies the services' tokens.
    key: TokenKey,
    /// Whether a token's stack and service exist now.
    known: Arc<dyn KnownServices + Send + Sync>,
    /// Where records and spans go.
    sink: LogSink,
}

impl OtlpReceiver {
    /// The largest body accepted as sent (compressed or not).
    pub const MAX_BODY: usize = 4 * 1024 * 1024;
    /// The largest body accepted once a gzip body is inflated.
    pub const MAX_DECODED: usize = 16 * 1024 * 1024;
    /// The only content type accepted: OTLP/HTTP's binary protobuf encoding.
    const PROTOBUF: &str = "application/x-protobuf";

    /// A receiver verifying tokens with `key` against the services `known` has, and offering
    /// what it receives to `sink`.
    pub fn new(key: TokenKey, known: Arc<dyn KnownServices + Send + Sync>, sink: LogSink) -> Self {
        Self(Arc::new(Inner { key, known, sink }))
    }

    /// The receiver's routes: `POST /v1/logs`, `POST /v1/traces`, and `/v1/metrics` as `404`.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/logs", post(Self::logs))
            .route("/v1/traces", post(Self::traces))
            .route("/v1/metrics", any(async || Refusal::NotFound))
            .layer(DefaultBodyLimit::max(Self::MAX_BODY))
            .with_state(self.clone())
    }

    /// `POST /v1/logs`.
    async fn logs(State(receiver): State<Self>, request: Request) -> Response {
        let (identity, export) = match receiver.accept::<ExportLogsServiceRequest>(request).await {
            Ok(accepted) => accepted,
            Err(refusal) => return refusal.into_response(),
        };
        let origin = Origin::from(identity);
        let delivery = receiver.deliver(origin.records(export).map(Ok), LogSink::try_record);
        Protobuf(ExportLogsServiceResponse::from(delivery)).into_response()
    }

    /// `POST /v1/traces`.
    async fn traces(State(receiver): State<Self>, request: Request) -> Response {
        let (identity, export) = match receiver.accept::<ExportTraceServiceRequest>(request).await {
            Ok(accepted) => accepted,
            Err(refusal) => return refusal.into_response(),
        };
        let origin = Origin::from(identity);
        let delivery = receiver.deliver(origin.spans(export), LogSink::try_span);
        Protobuf(ExportTraceServiceResponse::from(delivery)).into_response()
    }

    /// Authenticates the request, then reads and decodes its body as `M`. The token is checked
    /// before a byte of the body is read.
    async fn accept<M: Message + Default>(
        &self,
        request: Request,
    ) -> Result<(ServiceIdentity, M), Refusal> {
        let identity = self.authenticate(request.headers())?;
        let encoding = Encoding::try_from(request.headers())?;
        let body = Bytes::from_request(request, &())
            .await
            .map_err(|rejection| {
                if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    Refusal::TooLarge
                } else {
                    Refusal::Malformed
                }
            })?;
        let body = encoding.decode(body)?;
        let export = M::decode(body).map_err(|_| Refusal::Malformed)?;
        Ok((identity, export))
    }

    /// The service the bearer token names, if it verifies. Why it did not is logged at debug
    /// level, by the error alone: never the token or the header.
    fn authenticate(&self, headers: &HeaderMap) -> Result<ServiceIdentity, Refusal> {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                let (scheme, token) = value.split_once(' ')?;
                scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
            })
            .ok_or(Refusal::Unauthorized)?;
        presented
            .parse::<ServiceToken>()
            .and_then(|token| token.verify(&self.0.key, &*self.0.known))
            .map_err(|err| {
                tracing::debug!(error = %err, "OTLP export refused");
                Refusal::Unauthorized
            })
    }

    /// Offers each mapped item to the sink with `offer`, counting what was rejected: items
    /// that did not map and items the sink dropped. A per-service cap goes here, between the
    /// mapping and the sink, so what it drops is reported the same way.
    fn deliver<T>(
        &self,
        items: impl Iterator<Item = Result<T, InvalidSpan>>,
        offer: fn(&LogSink, T) -> bool,
    ) -> Delivery {
        let mut delivery = Delivery::default();
        for item in items {
            match item {
                Ok(item) => {
                    if !offer(&self.0.sink, item) {
                        delivery.dropped += 1;
                    }
                }
                Err(_) => delivery.invalid += 1,
            }
        }
        delivery
    }
}

/// How the request body is encoded, from `Content-Type` and `Content-Encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Identity,
    Gzip,
}

impl Encoding {
    /// The body as protobuf, inflated if it was gzipped.
    fn decode(self, body: Bytes) -> Result<Bytes, Refusal> {
        match self {
            Encoding::Identity => Ok(body),
            Encoding::Gzip => {
                let limit = u64::try_from(OtlpReceiver::MAX_DECODED).unwrap_or(u64::MAX);
                let mut inflated = Vec::new();
                GzDecoder::new(&body[..])
                    .take(limit + 1)
                    .read_to_end(&mut inflated)
                    .map_err(|_| Refusal::Malformed)?;
                if inflated.len() > OtlpReceiver::MAX_DECODED {
                    return Err(Refusal::TooLarge);
                }
                Ok(Bytes::from(inflated))
            }
        }
    }
}

/// `415` for any content type but protobuf, or a content coding other than gzip.
impl TryFrom<&HeaderMap> for Encoding {
    type Error = Refusal;

    fn try_from(headers: &HeaderMap) -> Result<Self, Self::Error> {
        let text = |name| {
            headers
                .get(name)
                .map(|value: &HeaderValue| value.to_str().map(str::trim))
        };
        let media_type = match text(header::CONTENT_TYPE) {
            Some(Ok(value)) => value.split(';').next().unwrap_or_default().trim(),
            _ => return Err(Refusal::UnsupportedMediaType),
        };
        if !media_type.eq_ignore_ascii_case(OtlpReceiver::PROTOBUF) {
            return Err(Refusal::UnsupportedMediaType);
        }
        match text(header::CONTENT_ENCODING) {
            None => Ok(Encoding::Identity),
            Some(Ok(coding)) if coding.is_empty() || coding.eq_ignore_ascii_case("identity") => {
                Ok(Encoding::Identity)
            }
            Some(Ok(coding)) if coding.eq_ignore_ascii_case("gzip") => Ok(Encoding::Gzip),
            Some(_) => Err(Refusal::UnsupportedMediaType),
        }
    }
}

/// What one export left undelivered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Delivery {
    /// Items the sink dropped: the recorder's channel was full, or there is no recorder.
    dropped: i64,
    /// Items that could not be mapped: spans without a valid trace or span id.
    invalid: i64,
}

impl Delivery {
    fn rejected(self) -> i64 {
        self.dropped + self.invalid
    }

    /// The partial success's message: what was rejected and why, without the payload.
    fn message(self) -> String {
        let mut reasons = Vec::new();
        if self.dropped > 0 {
            reasons.push(format!("{} dropped: the log store is busy", self.dropped));
        }
        if self.invalid > 0 {
            reasons.push(format!("{} without a valid trace or span id", self.invalid));
        }
        reasons.join("; ")
    }
}

impl From<Delivery> for ExportLogsServiceResponse {
    fn from(delivery: Delivery) -> Self {
        Self {
            partial_success: (delivery.rejected() > 0).then(|| ExportLogsPartialSuccess {
                rejected_log_records: delivery.rejected(),
                error_message: delivery.message(),
            }),
        }
    }
}

impl From<Delivery> for ExportTraceServiceResponse {
    fn from(delivery: Delivery) -> Self {
        Self {
            partial_success: (delivery.rejected() > 0).then(|| ExportTracePartialSuccess {
                rejected_spans: delivery.rejected(),
                error_message: delivery.message(),
            }),
        }
    }
}

/// A protobuf response body, `200` unless given another status.
struct Protobuf<M>(M);

impl<M: Message> IntoResponse for Protobuf<M> {
    fn into_response(self) -> Response {
        (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(OtlpReceiver::PROTOBUF),
            )],
            self.0.encode_to_vec(),
        )
            .into_response()
    }
}

/// Why a request was refused before anything was delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    /// No bearer token, or one that does not verify. The answer says no more.
    Unauthorized,
    /// Not protobuf, or not gzip.
    UnsupportedMediaType,
    /// Over [`OtlpReceiver::MAX_BODY`] or [`OtlpReceiver::MAX_DECODED`].
    TooLarge,
    /// Does not inflate or decode.
    Malformed,
    /// A signal hoster does not receive (metrics).
    NotFound,
}

impl Refusal {
    fn status(self) -> StatusCode {
        match self {
            Refusal::Unauthorized => StatusCode::UNAUTHORIZED,
            Refusal::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Refusal::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Refusal::Malformed => StatusCode::BAD_REQUEST,
            Refusal::NotFound => StatusCode::NOT_FOUND,
        }
    }
}

/// The `google.rpc.Status` OTLP/HTTP answers errors with: a gRPC code and a fixed message.
impl From<Refusal> for RpcStatus {
    fn from(refusal: Refusal) -> Self {
        let (code, message) = match refusal {
            Refusal::Unauthorized => (RpcStatus::UNAUTHENTICATED, ""),
            Refusal::UnsupportedMediaType => (
                RpcStatus::INVALID_ARGUMENT,
                "only application/x-protobuf, optionally gzip-encoded, is accepted",
            ),
            Refusal::TooLarge => (RpcStatus::RESOURCE_EXHAUSTED, "the body is too large"),
            Refusal::Malformed => (
                RpcStatus::INVALID_ARGUMENT,
                "the body is not an OTLP export request",
            ),
            Refusal::NotFound => (
                RpcStatus::UNIMPLEMENTED,
                "only logs and traces are received",
            ),
        };
        Self {
            code,
            message: message.to_owned(),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (self.status(), Protobuf(RpcStatus::from(self))).into_response()
    }
}

/// `google.rpc.Status` without its `details`, which hoster never sends.
#[derive(Clone, PartialEq, Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
}

impl RpcStatus {
    const INVALID_ARGUMENT: i32 = 3;
    const RESOURCE_EXHAUSTED: i32 = 8;
    const UNIMPLEMENTED: i32 = 12;
    const UNAUTHENTICATED: i32 = 16;
}
